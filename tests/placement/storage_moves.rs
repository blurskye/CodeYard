//! A repository on a storage node moves with its owner account to another cell.
use crate::support::native_database::PostgresDatabase;
use codeyard_db_directory::Directory;
use codeyard_router::directory;
use codeyard_security::node_identity::NodeKey;
use codeyard_types::cell_ids::Prefix;
use codeyard_types::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use std::time::Duration;

/// A repository on a storage node moves with its owner. The command copies it from the
/// placement primary; a failed load leaves the source serving and the next run continues
/// from the earlier copy; release removes the source's write journal rows.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_repository_on_a_storage_node_moves_from_its_primary() {
    const MAIN: &str = "refs/heads/main";
    let mut registry = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&registry.connection_url(), Default::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    let mut source = PostgresDatabase::new().await;
    let mut target = PostgresDatabase::new().await;
    for (database, prefix) in [(&source, 1), (&target, 2)] {
        codeyard_database::cells::initialize(&database.pool, Prefix::new(prefix))
            .await
            .unwrap();
        codeyard_database::cells::load(&database.pool)
            .await
            .unwrap();
    }
    let stores = super::quorum::Stores::new(&source.pool).await;
    let (pushed, _, errors) = stores.push(&source.pool).await;
    assert!(
        pushed.unwrap().success(),
        "{}",
        String::from_utf8_lossy(&errors)
    );
    let refs = stores.refs().await.swap_remove(0);
    let (owner, repo) = (stores.fixture.owner, stores.fixture.repo);
    let disk_path: String = sqlx::query_scalar("SELECT disk_path FROM repositories WHERE id=$1")
        .bind(repo)
        .fetch_one(&source.raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO issues(repo_id,number,author_id,title) VALUES($1,1,$2,'Moves along')")
        .bind(repo)
        .bind(owner)
        .execute(&source.raw)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "").unwrap();
    let source_root = root.path().join("one");
    let target_root = root.path().join("two");
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::create_dir_all(&target_root).unwrap();
    let state = root.path().join("state");
    let source_key = codeyard_security::node_identity::load_or_create(&state).unwrap();
    // The command copies with this cell's web node identity.
    codeyard_db_placement::cluster::register_own_node(
        &source.pool,
        &source_key.public_hex(),
        "web",
        &["web".to_owned()],
        "test",
    )
    .await
    .unwrap();
    let target_key = NodeKey::generate();
    let server_key = NodeKey::generate();
    for (prefix, key) in [(1, &source_key), (2, &target_key)] {
        directory
            .register_cell(Cell {
                id: Prefix::new(prefix),
                name: format!("cell-{prefix}"),
                region: "a".into(),
                state: State::Open,
                web_urls: vec![format!("http://127.0.0.1:{}/", 3000 + prefix)],
                ssh_addresses: vec![],
                rpc_addresses: vec![],
                node_keys: vec![key.public_hex()],
                database_bytes: 0,
                write_rate: 0,
                maximum_database_bytes: 1000000,
                maximum_write_rate: 100,
                read_regions: vec![],
            })
            .await
            .unwrap();
    }
    let listener = directory::Listener::bind(
        directory.clone(),
        &server_key,
        "127.0.0.1:0".parse().unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    for (object, keys) in [
        (
            Object {
                kind: ObjectKind::User,
                id: owner,
            },
            vec![Key::new(KeyKind::Name, "quorum-owner").unwrap()],
        ),
        (
            Object {
                kind: ObjectKind::Repository,
                id: repo,
            },
            vec![],
        ),
    ] {
        let nonce = "a".repeat(64);
        directory
            .reserve(&source_key.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
        directory
            .confirm(&source_key.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
    }
    sqlx::raw_sql("CREATE FUNCTION reject_move_issue() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected copy failure'; END $$; CREATE TRIGGER reject_move_issue BEFORE INSERT ON issues FOR EACH ROW EXECUTE FUNCTION reject_move_issue();")
        .execute(&target.raw).await.unwrap();
    let run = || {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_codeyard"));
        command
            .args([
                "cell",
                "move",
                "--kind",
                "user",
                "--name",
                "quorum-owner",
                "--target-database",
                &target.connection_url(),
                "--target-repositories",
                target_root.to_str().unwrap(),
                "--route-cache-seconds",
                "0",
                "--wait-seconds",
                "20",
            ])
            .current_dir(root.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("DATABASE_URL", source.connection_url())
            .env("GITSERV_STATE_DIR", &state)
            .env("REPOS_PATH", &source_root)
            .env("CLUSTER_TCP_BIND", "127.0.0.1:0")
            .env("DIRECTORY_ADDRESS", listener.address.to_string())
            .env("DIRECTORY_PUBLIC_KEY", server_key.public_hex())
            .kill_on_drop(true);
        command
    };
    let failed = tokio::time::timeout(Duration::from_secs(120), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains("injected copy failure"), "{stderr}");
    let destination = target_root.join(std::path::Path::new(&disk_path).file_name().unwrap());
    assert!(
        destination.join("HEAD").is_file(),
        "the copy is kept for the next run"
    );
    let view = codeyard_db_placement::cluster::writes::recovery_view(&source.pool, repo)
        .await
        .unwrap();
    assert!(view.serving, "a failed move leaves the source serving");
    let unfinished: i64 = sqlx::query_scalar("SELECT count(*) FROM account_moves")
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert_eq!(unfinished, 0);
    sqlx::query("DROP TRIGGER reject_move_issue ON issues")
        .execute(&target.raw)
        .await
        .unwrap();
    let moved = tokio::time::timeout(Duration::from_secs(120), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        moved.status.success(),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );
    let copy = git2::Repository::open_bare(&destination).unwrap();
    assert_eq!(
        copy.find_reference(MAIN)
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        refs[MAIN]
    );
    assert_eq!(
        copy.find_reference("HEAD").unwrap().symbolic_target(),
        Some(MAIN)
    );
    drop(copy);
    assert!(!target_root
        .join(format!(
            ".move-{}",
            destination.file_name().unwrap().to_str().unwrap()
        ))
        .exists());
    let (path, node): (String, Option<i64>) =
        sqlx::query_as("SELECT disk_path,storage_node_id FROM repositories WHERE id=$1")
            .bind(repo)
            .fetch_one(&target.raw)
            .await
            .unwrap();
    assert_eq!((path.as_str(), node), (destination.to_str().unwrap(), None));
    let stub: bool = sqlx::query_scalar("SELECT is_stub FROM repositories WHERE id=$1")
        .bind(repo)
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert!(stub);
    for table in [
        "repository_writes",
        "repository_voters",
        "repository_write_control",
        "repository_committed_refs",
    ] {
        let left: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE repository_id=$1"
        )))
        .bind(repo)
        .fetch_one(&source.raw)
        .await
        .unwrap();
        assert_eq!(left, 0, "{table} kept rows of a moved repository");
    }
    let issue: String = sqlx::query_scalar("SELECT title FROM issues WHERE repo_id=$1")
        .bind(repo)
        .fetch_one(&target.raw)
        .await
        .unwrap();
    assert_eq!(issue, "Moves along");
    let clone = root.path().join("clone");
    let cloned = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("git")
            .arg("clone")
            .arg(&destination)
            .arg(&clone)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    stores.stop().await;
    listener.stop().await;
    directory.close().await;
    target.finish().await;
    source.finish().await;
    registry.finish().await;
}
