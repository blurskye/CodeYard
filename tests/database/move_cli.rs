//! The operator move runs against two real cells and a pinned directory, then Git and
//! HTTP read the preserved repository and issue from the destination.
use crate::support::native_database::PostgresDatabase;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use codeyard_model::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use codeyard_support::{
    cells::Prefix,
    node_identity::{self, NodeKey},
};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

#[tokio::test]
async fn cli_move_rolls_back_a_failed_load_then_preserves_git_and_issue_ids() {
    crate::support::fake_webui_bundle::install();
    let mut source = PostgresDatabase::new().await;
    let mut target = PostgresDatabase::new().await;
    for (database, prefix) in [(&source, 1), (&target, 2)] {
        codeyard_db_core::cells::initialize(&database.pool, Prefix::new(prefix))
            .await
            .unwrap();
        codeyard_db_core::cells::load(&database.pool).await.unwrap();
    }
    let mut registry = PostgresDatabase::unmigrated().await;
    let directory =
        codeyard_db_directory::Directory::connect(&registry.connection_url(), Default::default())
            .await
            .unwrap();
    directory.migrate().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "").unwrap();
    let source_root = root.path().join("one");
    let target_root = root.path().join("two");
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::create_dir_all(&target_root).unwrap();
    let state = root.path().join("state");
    let source_key = node_identity::load_or_create(&state).unwrap();
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
    let listener = codeyard_cells_router::directory::Listener::bind(
        directory.clone(),
        &server_key,
        "127.0.0.1:0".parse().unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    let directory_address = listener.address;
    let alice = codeyard_db_identity::users::accounts::create_user(&source.pool, "alice", None)
        .await
        .unwrap();
    let disk = source_root.join(format!("{}.git", uuid::Uuid::new_v4()));
    let git = git2::Repository::init_bare(&disk).unwrap();
    let tree_id = git.treebuilder(None).unwrap().write().unwrap();
    let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
    let oid = git
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Before the move",
            &git.find_tree(tree_id).unwrap(),
            &[],
        )
        .unwrap();
    git.set_head("refs/heads/main").unwrap();
    drop(git);
    let repo = codeyard_db_repos::repositories::records::create_repository(
        &source.pool,
        alice.id,
        "project",
        None,
        false,
        disk.to_str().unwrap(),
    )
    .await
    .unwrap();
    let issue: i64 = sqlx::query_scalar("INSERT INTO issues(repo_id,number,author_id,title) VALUES($1,7,$2,'Preserved issue') RETURNING id")
        .bind(repo.id).bind(alice.id).fetch_one(&source.raw).await.unwrap();
    let objects = [
        Object {
            kind: ObjectKind::User,
            id: alice.id,
        },
        Object {
            kind: ObjectKind::Repository,
            id: repo.id,
        },
    ];
    for object in objects {
        let keys = if object.kind == ObjectKind::User {
            vec![Key::new(KeyKind::Name, "alice").unwrap()]
        } else {
            vec![]
        };
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
    // Fail after the account/repository rows have been staged, not during preflight.
    sqlx::raw_sql("CREATE FUNCTION reject_move_issue() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected copy failure'; END $$; CREATE TRIGGER reject_move_issue BEFORE INSERT ON issues FOR EACH ROW EXECUTE FUNCTION reject_move_issue();")
        .execute(&target.raw).await.unwrap();
    let run = || {
        let mut command =
            tokio::process::Command::new(std::env::var_os("GITSERV_TEST_OPENSSH_HELPER").unwrap());
        command
            .args([
                "cell",
                "move",
                "--kind",
                "user",
                "--name",
                "alice",
                "--target-database",
                &target.connection_url(),
                "--target-repositories",
                target_root.to_str().unwrap(),
                "--route-cache-seconds",
                "0",
                "--wait-seconds",
                "10",
            ])
            .current_dir(root.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("DATABASE_URL", source.connection_url())
            .env("GITSERV_STATE_DIR", &state)
            .env("REPOS_PATH", &source_root)
            .env("DIRECTORY_ADDRESS", directory_address.to_string())
            .env("DIRECTORY_PUBLIC_KEY", server_key.public_hex())
            .kill_on_drop(true);
        command
    };
    // An incoming move must share the destination's move slot with an outgoing one.
    let destination_lease =
        codeyard_db_cluster::cluster::serving_lock(&target.pool, "account-move")
            .await
            .unwrap();
    let busy = tokio::time::timeout(Duration::from_secs(30), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!busy.status.success());
    assert!(
        String::from_utf8_lossy(&busy.stderr).contains("another server"),
        "{}",
        String::from_utf8_lossy(&busy.stderr)
    );
    assert!(std::fs::read_dir(&target_root).unwrap().next().is_none());
    destination_lease.release().await;
    let failed = tokio::time::timeout(Duration::from_secs(90), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !failed.status.success(),
        "the injected target failure must fail the move"
    );
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("injected copy failure"),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    for object in objects {
        assert_eq!(
            directory.object_home(object).await.unwrap(),
            Some(Prefix::new(1))
        );
    }
    let native: bool = sqlx::query_scalar("SELECT NOT is_stub FROM users WHERE id=$1")
        .bind(alice.id)
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert!(native);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM repositories WHERE id=$1")
        .bind(repo.id)
        .fetch_one(&target.raw)
        .await
        .unwrap();
    assert_eq!(count, 0, "a failed load rolls back all target rows");
    // A retry must refuse a destination that became active before touching its Git,
    // not merely discover that conflict when it eventually loads the database rows.
    let destination = target_root.join(disk.file_name().unwrap());
    sqlx::query("INSERT INTO users(id,username,home_cell) VALUES($1,'alice',2)")
        .bind(alice.id)
        .execute(&target.raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO repositories(id,owner_kind,owner_user_id,name,disk_path,home_cell) VALUES($1,'user',$2,'project',$3,2)")
        .bind(repo.id).bind(alice.id).bind(destination.to_str().unwrap())
        .execute(&target.raw).await.unwrap();
    let active_git = git2::Repository::open_bare(&destination).unwrap();
    let parent = active_git.find_commit(oid).unwrap();
    let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
    let active_oid = active_git
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Destination-only write",
            &parent.tree().unwrap(),
            &[&parent],
        )
        .unwrap();
    drop(parent);
    drop(active_git);
    let active = tokio::time::timeout(Duration::from_secs(30), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!active.status.success());
    assert!(
        String::from_utf8_lossy(&active.stderr).contains("target account is already active"),
        "{}",
        String::from_utf8_lossy(&active.stderr)
    );
    assert_eq!(
        git2::Repository::open_bare(&destination)
            .unwrap()
            .head()
            .unwrap()
            .target(),
        Some(active_oid)
    );
    let move_count: i64 = sqlx::query_scalar("SELECT count(*) FROM account_moves")
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert_eq!(
        move_count, 0,
        "destination refusal must unfreeze the source"
    );
    sqlx::query("DELETE FROM repositories WHERE id=$1")
        .bind(repo.id)
        .execute(&target.raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(alice.id)
        .execute(&target.raw)
        .await
        .unwrap();
    std::fs::remove_dir_all(&destination).unwrap();
    sqlx::query("UPDATE issues SET title='Preserved issue after retry' WHERE id=$1")
        .bind(issue)
        .execute(&source.raw)
        .await
        .unwrap();
    sqlx::query("DROP TRIGGER reject_move_issue ON issues")
        .execute(&target.raw)
        .await
        .unwrap();
    // The destination can already have accepted writes when the directory reply is
    // lost. A retry must finish switching, never replace that active copy.
    listener.stop().await;
    let interrupted = tokio::time::timeout(Duration::from_secs(90), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!interrupted.status.success());
    let phase: String = sqlx::query_scalar(
        "SELECT state FROM account_moves WHERE account_kind='user' AND account_id=$1",
    )
    .bind(alice.id)
    .fetch_one(&source.raw)
    .await
    .unwrap();
    assert_eq!(
        phase,
        "switched",
        "{}",
        String::from_utf8_lossy(&interrupted.stderr)
    );
    sqlx::query("UPDATE issues SET title='Preserved issue after switch retry' WHERE id=$1")
        .bind(issue)
        .execute(&target.raw)
        .await
        .unwrap();
    let listener = codeyard_cells_router::directory::Listener::bind(
        directory.clone(),
        &server_key,
        directory_address,
        Default::default(),
    )
    .await
    .unwrap();
    let moved = tokio::time::timeout(Duration::from_secs(90), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        moved.status.success(),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );
    for object in objects {
        assert_eq!(
            directory.object_home(object).await.unwrap(),
            Some(Prefix::new(2))
        );
    }
    assert!(
        !disk.exists(),
        "only the switched and released source Git copy is removed"
    );
    let destination = target_root.join(disk.file_name().unwrap());
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
    assert_eq!(
        git2::Repository::open(&clone)
            .unwrap()
            .head()
            .unwrap()
            .target(),
        Some(oid)
    );
    codeyard_db_cluster::node_config::upsert_node_configuration(&target.pool, "public", true)
        .await
        .unwrap();
    let store = Arc::new(
        codeyard_storage::ObjectStore::open(root.path(), None)
            .await
            .unwrap(),
    );
    let app = codeyard_api_rest::create_router(codeyard_app::app::AppState::new(
        target.pool.clone(),
        target_root,
        store,
        None,
        Default::default(),
        None,
    ));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v3/repos/alice/project/issues/7")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(body["id"], issue);
    assert_eq!(body["number"], 7);
    assert_eq!(body["title"], "Preserved issue after switch retry");
    listener.stop().await;
    directory.close().await;
    registry.finish().await;
    source.finish().await;
    target.finish().await;
}
