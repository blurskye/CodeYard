//! Operator commands restore only disposable databases and directories owned by this fixture.
use codeyard_security::node_identity::NodeKey;
use std::path::Path;

pub(super) fn command(
    root: &Path,
    url: &str,
    key: Option<&NodeKey>,
    name: &str,
    repos: &Path,
    state: &Path,
    args: &[&str],
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_codeyard"));
    command
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("LANG", "C.UTF-8")
        .env("DATABASE_URL", url)
        .env("GITSERV_STATE_DIR", state)
        .env("REPOS_PATH", repos)
        .env("NODE_NAME", name)
        .env("CLUSTER_TCP_BIND", "127.0.0.1:0")
        .env("GITSERV_NODE_ROLES", "web")
        .env("SECRETS_MASTER_KEY", "01".repeat(32))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(key) = key {
        command.env("NODE_KEY", key.secret_hex());
    }
    command
}

pub(super) async fn run_command(command: tokio::process::Command) -> std::process::Output {
    codeyard_git::repository::process::run(
        command,
        &[],
        1024 * 1024,
        std::time::Duration::from_secs(120),
    )
    .await
    .unwrap()
}
pub(super) async fn cli(
    root: &Path,
    url: &str,
    key: Option<&NodeKey>,
    name: &str,
    repos: &Path,
    state: &Path,
    args: &[&str],
) -> std::process::Output {
    run_command(command(root, url, key, name, repos, state, args)).await
}

fn success(output: std::process::Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
fn commit(path: &Path) -> String {
    let repo = git2::Repository::init_bare(path).unwrap();
    let tree = repo
        .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let signature = git2::Signature::now("Backup", "backup@example.test").unwrap();
    let oid = repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Backup fixture",
            &tree,
            &[],
        )
        .unwrap();
    repo.set_head("refs/heads/main").unwrap();
    oid.to_string()
}

#[tokio::test]
async fn sqlite_backup_and_restore_cli_keep_repository_and_account_data() {
    let mut db = crate::support::database::Database::new(false).await;
    db.complete_setup().await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "").unwrap();
    let repos = root.path().join("source-repos");
    std::fs::create_dir(&repos).unwrap();
    let path = repos.join(format!("{}.git", uuid::Uuid::new_v4()));
    let tip = commit(&path);
    let user = db.actor("sqlite-backup-owner").await;
    let repository = codeyard_db_repos::repositories::records::create_repository(
        db.pool(),
        user,
        "saved",
        None,
        false,
        path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let backup = root.path().join("backup");
    success(
        cli(
            root.path(),
            &db.connection_url(),
            None,
            "source",
            &repos,
            &root.path().join("source-state"),
            &["backup", backup.to_str().unwrap()],
        )
        .await,
    );
    let restored_repos = root.path().join("restored-repos");
    let restored_state = root.path().join("restored-state");
    let url = format!(
        "sqlite://{}?mode=rwc",
        root.path().join("restored.db").display()
    );
    success(
        cli(
            root.path(),
            &url,
            None,
            "restored",
            &restored_repos,
            &restored_state,
            &["restore", backup.to_str().unwrap()],
        )
        .await,
    );
    success(
        cli(
            root.path(),
            &url,
            None,
            "restored",
            &restored_repos,
            &restored_state,
            &["repo", "verify", "--repair"],
        )
        .await,
    );
    let restored = codeyard_database::init_db(&url).await.unwrap();
    let row =
        codeyard_db_repos::repositories::records::get_repository_by_id(&restored, repository.id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(row.owner_id(), user);
    assert_eq!(
        git2::Repository::open_bare(row.disk_path)
            .unwrap()
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        tip
    );
    restored.close().await;
    db.finish().await;
}

#[tokio::test]
async fn cluster_backup_cli_restores_into_an_empty_cluster_and_resumes_an_interrupted_node() {
    use codeyard_git_storage::context::NodeContext;
    use codeyard_nodes::transport::{IrohDialer, Relay};
    use codeyard_storage::{ObjectStore, Upload};
    use sha2::Digest;
    use std::sync::Arc;
    // The builder helper owns this whole disposable server. Database names are generated here.
    std::env::var("GITSERV_TEST_POSTGRES_MARKER")
        .expect("builder-test.sh owns the PostgreSQL fixture");
    let admin_url = std::env::var("GITSERV_TEST_DATABASE_URL").unwrap();
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .map(codeyard_database::pool::Pool::from)
        .unwrap();
    let mut urls = Vec::new();
    let mut names = Vec::new();
    for _ in 0..2 {
        let name = format!("codeyard_backup_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .unwrap();
        let mut url = url::Url::parse(&admin_url).unwrap();
        url.set_path(&name);
        url.set_query(None);
        names.push(name);
        urls.push(url.to_string());
    }
    let source = codeyard_database::init_db(&urls[0]).await.unwrap();
    codeyard_database::run_migrations(&source).await.unwrap();
    let stores = super::quorum::Stores::new(&source).await;
    let (pushed, output, errors) = stores.push(&source).await;
    assert!(pushed.unwrap().success(), "{output:?} {errors:?}");
    let record = codeyard_db_repos::repositories::records::get_repository_by_id(
        &source,
        stores.fixture.repo,
    )
    .await
    .unwrap()
    .unwrap();
    let expected = codeyard_db_placement::cluster::writes::recovery_view(&source, record.id)
        .await
        .unwrap()
        .refs
        .into_iter()
        .find(|value| value.name == "refs/heads/main")
        .unwrap()
        .target;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "").unwrap();
    let key = NodeKey::generate();
    let member = codeyard_db_placement::cluster::register_own_node(
        &source,
        &key.public_hex(),
        "backup-web",
        &["web".into()],
        "test",
    )
    .await
    .unwrap();
    let context = NodeContext {
        pool: Some(source.clone()),
        rpc: Arc::new(codeyard_repos_rpc::RpcFactory),
        iroh: IrohDialer::new(Relay::Off, key.clone()),
        key: key.clone(),
        node_id: member.id,
        repos_path: root.path().join("source-web-repos"),
        peers: Default::default(),
        unreachable: Default::default(),
        clone_cursor: Default::default(),
        ci_replicas: Default::default(),
    };
    for node in &stores.contexts {
        let codeyard_database::DbPool::Postgres(raw) = &source else {
            unreachable!()
        };
        sqlx::query("UPDATE nodes SET disk_free=900000 WHERE id=$1")
            .bind(node.node_id)
            .execute(raw)
            .await
            .unwrap();
        codeyard_db_placement::drain::record_capacity(&source, &node.key.public_hex(), 1_000_000)
            .await
            .unwrap();
    }
    let bytes = b"artifact bytes survive the complete cluster";
    let digest = hex::encode(sha2::Sha256::digest(bytes));
    let objects = ObjectStore::on_nodes(
        &root.path().join("source-state"),
        codeyard_git_storage::objects::Store::new(context.clone()),
    )
    .await
    .unwrap();
    objects
        .put_bytes(
            &Upload {
                key: "uploads/restore-fixture",
                bytes: bytes.len() as i64,
                content_type: "text/plain",
                sha256_hex: &digest,
            },
            bytes.to_vec(),
            Arc::new(objects.try_acquire_io().unwrap()),
        )
        .await
        .unwrap();
    let backup = root.path().join("backup");
    success(
        cli(
            root.path(),
            &urls[0],
            Some(&key),
            "backup-web",
            &context.repos_path,
            &root.path().join("source-state"),
            &["backup", backup.to_str().unwrap()],
        )
        .await,
    );
    stores.stop().await;
    source.close().await;
    context.iroh.close().await;
    let coordinator = root.path().join("restored-web-repos");
    let state = root.path().join("restored-web-state");
    let archive = backup.join("database.pgdump");
    let held_archive = backup.join("database.pgdump.held");
    std::fs::rename(&archive, &held_archive).unwrap();
    std::fs::write(&archive, b"not a PostgreSQL archive").unwrap();
    let output = cli(
        root.path(),
        &urls[1],
        Some(&key),
        "backup-web",
        &coordinator,
        &state,
        &["restore", backup.to_str().unwrap()],
    )
    .await;
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("pg_restore"), "{error}");
    let empty = codeyard_database::init_db(&urls[1]).await.unwrap();
    assert!(codeyard_db_placement::backups::is_empty(&empty)
        .await
        .unwrap());
    empty.close().await;
    std::fs::remove_file(&archive).unwrap();
    std::fs::rename(held_archive, archive).unwrap();
    let missing = backup.join("repositories").join(format!(
        "{}.git",
        codeyard_git_storage::wire::repository_uuid(&record.disk_path).unwrap()
    ));
    let held = missing.with_extension("held");
    std::fs::rename(&missing, &held).unwrap();
    let output = cli(
        root.path(),
        &urls[1],
        Some(&key),
        "backup-web",
        &coordinator,
        &state,
        &["restore", backup.to_str().unwrap()],
    )
    .await;
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    // The fence below distinguishes a file-stage interruption from a rolled-back
    // database import. The OS may report the missing Git directory as ENOENT.
    let interrupted = codeyard_database::init_db(&urls[1]).await.unwrap();
    assert!(
        codeyard_db_placement::backups::restore_pending(&interrupted)
            .await
            .unwrap()
    );
    let unrelated = root.path().join("unrelated-repos");
    std::fs::create_dir(&unrelated).unwrap();
    std::fs::write(unrelated.join("keep"), b"preserve").unwrap();
    assert!(!cli(
        root.path(),
        &urls[1],
        Some(&key),
        "backup-web",
        &unrelated,
        &state,
        &["restore", backup.to_str().unwrap()]
    )
    .await
    .status
    .success());
    assert_eq!(std::fs::read(unrelated.join("keep")).unwrap(), b"preserve");
    interrupted.close().await;
    std::fs::rename(held, missing).unwrap();
    success(
        cli(
            root.path(),
            &urls[1],
            Some(&key),
            "backup-web",
            &coordinator,
            &state,
            &["restore", backup.to_str().unwrap()],
        )
        .await,
    );
    let restored = codeyard_database::init_db(&urls[1]).await.unwrap();
    assert!(codeyard_db_placement::backups::restore_pending(&restored)
        .await
        .unwrap());
    let premature = cli(
        root.path(),
        &urls[1],
        Some(&key),
        "backup-web",
        &coordinator,
        &state,
        &["repo", "verify", "--repair"],
    )
    .await;
    assert!(!premature.status.success());
    for (index, node) in stores.contexts.iter().enumerate() {
        let repos = root.path().join(format!("restored-store-{index}"));
        let state = root.path().join(format!("restored-store-state-{index}"));
        if index == 0 {
            let wrong = NodeKey::generate();
            assert!(!cli(
                root.path(),
                &urls[1],
                Some(&wrong),
                "storage",
                &repos,
                &state,
                &["restore", backup.to_str().unwrap(), "--node"]
            )
            .await
            .status
            .success());
            let source = backup.join("repositories").join(format!(
                "{}.git",
                codeyard_git_storage::wire::repository_uuid(&record.disk_path).unwrap()
            ));
            let held = source.with_extension("held");
            std::fs::rename(&source, &held).unwrap();
            assert!(!cli(
                root.path(),
                &urls[1],
                Some(&node.key),
                "storage",
                &repos,
                &state,
                &["restore", backup.to_str().unwrap(), "--node"]
            )
            .await
            .status
            .success());
            assert!(codeyard_db_placement::backups::restore_pending(&restored)
                .await
                .unwrap());
            std::fs::rename(held, source).unwrap();
        }
        success(
            cli(
                root.path(),
                &urls[1],
                Some(&node.key),
                "storage",
                &repos,
                &state,
                &["restore", backup.to_str().unwrap(), "--node"],
            )
            .await,
        );
    }
    success(
        cli(
            root.path(),
            &urls[1],
            Some(&key),
            "backup-web",
            &coordinator,
            &state,
            &["repo", "verify", "--repair"],
        )
        .await,
    );
    assert!(!codeyard_db_placement::backups::restore_pending(&restored)
        .await
        .unwrap());
    let uuid = codeyard_git_storage::wire::repository_uuid(&record.disk_path).unwrap();
    let view = codeyard_db_placement::cluster::writes::recovery_view(&restored, record.id)
        .await
        .unwrap();
    for (index, node) in stores.contexts.iter().enumerate() {
        let repos = root.path().join(format!("restored-store-{index}"));
        let path = codeyard_git_storage::context::term_path(&repos, uuid, view.term);
        assert_eq!(
            git2::Repository::open_bare(path)
                .unwrap()
                .find_reference("refs/heads/main")
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            expected
        );
        let declaration = codeyard_storage::nodes::Declaration {
            key: "uploads/restore-fixture".into(),
            digest: digest.clone(),
            bytes: bytes.len() as i64,
            content_type: "text/plain".into(),
        };
        assert_eq!(
            std::fs::read(repos.join("objects").join(declaration.address().unwrap())).unwrap(),
            bytes
        );
        assert_eq!(
            codeyard_db_placement::objects::copies(&restored, &declaration.key)
                .await
                .unwrap()
                .iter()
                .find(|copy| copy.0 == node.node_id)
                .unwrap()
                .1,
            1
        );
    }
    // Start real storage sockets against the restored database and read both Git and bytes.
    let mut listeners = Vec::new();
    let mut contexts = Vec::new();
    for (index, old) in stores.contexts.iter().enumerate() {
        let context = NodeContext {
            pool: Some(restored.clone()),
            rpc: Arc::new(codeyard_repos_rpc::RpcFactory),
            iroh: IrohDialer::new(Relay::Off, old.key.clone()),
            key: old.key.clone(),
            node_id: old.node_id,
            repos_path: root.path().join(format!("restored-store-{index}")),
            peers: Default::default(),
            unreachable: Default::default(),
            clone_cursor: Default::default(),
            ci_replicas: Default::default(),
        };
        listeners.push(super::objects::listen(&context).await);
        contexts.push(context);
    }
    for context in &contexts {
        codeyard_placement::replicas::promote::recover_family(
            &restored,
            record.id,
            context.node_id,
            &context.repos_path,
        )
        .await
        .unwrap();
    }
    let web = NodeContext {
        pool: Some(restored.clone()),
        rpc: Arc::new(codeyard_repos_rpc::RpcFactory),
        iroh: IrohDialer::new(Relay::Off, key.clone()),
        key: key.clone(),
        node_id: member.id,
        repos_path: coordinator.clone(),
        peers: Default::default(),
        unreachable: Default::default(),
        clone_cursor: Default::default(),
        ci_replicas: Default::default(),
    };
    let want = format!("want {expected}\n");
    let request = format!("{:04x}{want}00000009done\n", want.len() + 4);
    let mut pack = Vec::new();
    let mut bounded =
        codeyard_git_storage::git_transport::LimitedWriter::new(&mut pack, 1024 * 1024);
    let (_, status) = web
        .git_session(
            record.storage_node_id.unwrap(),
            codeyard_git_storage::wire::Request {
                repository: uuid,
                operation: codeyard_git_storage::wire::Operation::GitSession(
                    codeyard_git_storage::store_types::GitSession::new(
                        codeyard_git_storage::store_types::GitService::UploadPack,
                        true,
                    ),
                ),
                actor: None,
                ci_run: None,
            },
            codeyard_git_storage::store_types::GitStreams {
                input: &mut request.as_bytes(),
                output: &mut bounded,
                errors: &mut tokio::io::sink(),
            },
        )
        .await
        .unwrap();
    assert!(status.success());
    assert!(pack.windows(4).any(|bytes| bytes == b"PACK"));
    use codeyard_storage::nodes::NodeStore;
    let (_, downloaded) = codeyard_git_storage::objects::Store::new(web.clone())
        .get("uploads/restore-fixture", 0, None)
        .await
        .unwrap();
    assert_eq!(downloaded, bytes);
    for listener in listeners {
        listener.stop().await;
    }
    for context in contexts {
        context.iroh.close().await;
    }
    web.iroh.close().await;
    restored.close().await;
    drop(stores);
    for name in names {
        sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .execute(&admin)
            .await
            .unwrap();
    }
    admin.close().await;
}

#[tokio::test]
async fn offline_maintenance_and_serving_leases_exclude_each_other_on_both_engines() {
    for pg in [false, true] {
        let mut db = crate::support::database::Database::new(pg).await;
        let serving = codeyard_db_placement::cluster::serving_lock(db.pool(), "restore-lease")
            .await
            .unwrap();
        assert!(codeyard_db_placement::cluster::maintenance_lock(db.pool())
            .await
            .is_err());
        serving.release().await;
        let maintenance = codeyard_db_placement::cluster::maintenance_lock(db.pool())
            .await
            .unwrap();
        assert!(
            codeyard_db_placement::cluster::serving_lock(db.pool(), "another-core")
                .await
                .is_err()
        );
        maintenance.release().await;
        db.finish().await;
    }
}
