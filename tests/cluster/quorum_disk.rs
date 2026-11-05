//! An owned, bounded tmpfs produces real ENOSPC without filling the builder's disk.
use crate::support::database::Database;
use std::{io::Write, time::Duration};

async fn run(mut command: tokio::process::Command) -> std::process::Output {
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    codeyard_git::repository::process::run(command, &[], 1024 * 1024, Duration::from_secs(90))
        .await
        .unwrap()
}

async fn bind_readonly(source: &std::path::Path, target: &std::path::Path) {
    if source.is_dir() {
        std::fs::create_dir_all(target).unwrap();
    } else {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::File::create(target).unwrap();
    }
    let bind = if source.is_dir() { "--rbind" } else { "--bind" };
    for arguments in [vec![bind], vec!["-o", "remount,bind,ro"]] {
        let mut command = tokio::process::Command::new("mount");
        command.arg("--no-mtab");
        command.args(&arguments);
        if arguments[0] == bind {
            command.arg(source);
        }
        let output = run({
            command.arg(target);
            command
        })
        .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Host root-owned ancestors are unmapped in a rootless user namespace. Keep the
/// normal private-path checks by giving this fixture its own mapped, owned root.
async fn enter_private_root(root: &std::path::Path) {
    use std::os::unix::process::CommandExt;
    let sandbox = root.join("root");
    std::fs::create_dir(&sandbox).unwrap();
    for source in [
        "/nix/store",
        "/bin",
        "/usr",
        "/lib",
        "/lib64",
        "/dev",
        "/proc",
    ] {
        let source = std::path::Path::new(source);
        if source.exists() {
            bind_readonly(source, &sandbox.join(source.strip_prefix("/").unwrap())).await;
        }
    }
    let executable = std::env::current_exe().unwrap();
    let mut binaries = std::collections::BTreeSet::from([
        executable.clone(),
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_codeyard")),
    ]);
    for name in ["GITSERV_TEST_OPENSSH_HELPER", "CODEYARD_ACTIONS_BIN"] {
        if let Some(binary) = std::env::var_os(name) {
            binaries.insert(binary.into());
        }
    }
    for source in binaries {
        assert!(source.is_absolute() && source.is_file());
        let target = sandbox.join(source.strip_prefix("/").unwrap());
        if !target.is_file() {
            bind_readonly(&source, &target).await;
        }
    }
    for name in ["tmp", "full"] {
        std::fs::create_dir(sandbox.join(name)).unwrap();
    }
    // The Git broker validates its executable's ancestors as well. A bind of
    // the host Nix store retains unmapped root ownership, so copy this trusted
    // tool into the fixture's mapped root instead of relaxing that validation.
    let tools = sandbox.join("tools");
    std::fs::create_dir(&tools).unwrap();
    let git = codeyard_git::repository::process::executable().unwrap();
    assert!(std::fs::metadata(&git).unwrap().len() <= 64 * 1024 * 1024);
    std::fs::copy(git, tools.join("git")).unwrap();
    let mut paths = vec![std::path::PathBuf::from("/tools")];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(paths).unwrap();
    let mut command = tokio::process::Command::new("mount");
    command.arg("--no-mtab");
    let output = run({
        command
            .args(["-t", "tmpfs", "-o", "size=4m,nosuid,nodev", "tmpfs"])
            .arg(sandbox.join("full"));
        command
    })
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Exec keeps the original child PID and the parent's process-group deadline.
    let error = std::process::Command::new("chroot")
        .arg(&sandbox)
        .arg(executable)
        .args([
            "--exact",
            "quorum_disk::full_filesystem_refuses_a_push_and_recovers_after_freeing_space",
            "--nocapture",
        ])
        .env("CODEYARD_OWNED_FULL_DISK", "/full")
        .env("CODEYARD_FULL_DISK_PRIVATE_ROOT", "1")
        .env("TMPDIR", "/tmp")
        .env("PATH", path)
        .exec();
    panic!("enter owned full-disk fixture: {error}");
}

#[test]
fn full_filesystem_refuses_a_push_and_recovers_after_freeing_space() {
    crate::support::large_stack::run(|| async {
        let Some(root) = std::env::var_os("CODEYARD_OWNED_FULL_DISK") else {
            let directory = tempfile::tempdir().unwrap();
            let mut command = tokio::process::Command::new("unshare");
            command
                .args(["--user", "--map-root-user", "--mount"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "quorum_disk::full_filesystem_refuses_a_push_and_recovers_after_freeing_space",
                    "--nocapture",
                ])
                .env("CODEYARD_OWNED_FULL_DISK", directory.path())
                .env_remove("CODEYARD_FULL_DISK_PRIVATE_ROOT");
            let output = run(command).await;
            assert!(
                output.status.success(),
                "Full-disk acceptance requires an owned user/mount namespace: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        };
        let root = std::path::PathBuf::from(root);
        if std::env::var_os("CODEYARD_FULL_DISK_PRIVATE_ROOT").is_none() {
            enter_private_root(&root).await;
            unreachable!("the private root replaces this process");
        }
        // This test process owns the mount namespace. Exiting unmounts it even on panic;
        // the parent's TempDir then removes the empty mount point.
        let original = std::env::var_os("TMPDIR");
        for pg in [false, true] {
            let mut database = Database::empty(pg).await;
            std::env::set_var("TMPDIR", &root);
            let stores = super::quorum::Stores::new(database.pool()).await;
            super::quorum_sql::set_timeout(database.pool(), 1)
                .await
                .unwrap();
            let before = stores.refs().await;
            let input = stores.push_input().await;
            let mut filler = std::fs::File::create(root.join("owned-filler")).unwrap();
            let block = [0u8; 16 * 1024];
            let mut written = 0;
            loop {
                assert!(written < 8 * 1024 * 1024, "the 4 MiB mount was not bounded");
                match filler.write(&block) {
                    Ok(count) if count > 0 => written += count,
                    Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => break,
                    other => panic!("unexpected filler write: {other:?}"),
                }
            }
            assert_eq!(codeyard_cluster::disk_free(&root).unwrap(), 0);
            for context in &stores.contexts {
                let node =
                    codeyard_db_cluster::cluster::get_node_by_id(database.pool(), context.node_id)
                        .await
                        .unwrap()
                        .unwrap();
                codeyard_db_cluster::cluster::record_node_heartbeat(
                    database.pool(),
                    &context.key.public_hex(),
                    "test",
                    node.addr_hint.as_deref().unwrap(),
                    Some(0),
                )
                .await
                .unwrap();
            }
            assert!(
                codeyard_repos_core::cluster::placement::storage_node(
                    Some(&stores.contexts[0]),
                    None,
                )
                .await
                .is_err(),
                "full nodes must not receive new repositories"
            );
            let (status, output, errors) = stores.push_with_input(database.pool(), input).await;
            assert!(status.is_err() || !output.is_empty() || !errors.is_empty());
            assert!(!String::from_utf8_lossy(&output).contains("ok refs/heads/main"));
            assert_eq!(stores.refs().await, before);
            assert_eq!(
                codeyard_db_cluster::cluster::writes::recovery_view(
                    database.pool(),
                    stores.fixture.repo
                )
                .await
                .unwrap()
                .committed_version,
                0
            );
            filler.set_len(0).unwrap();
            drop(filler);
            std::fs::remove_file(root.join("owned-filler")).unwrap();
            let free = codeyard_cluster::disk_free(&root).unwrap();
            assert!(free > 0);
            for context in &stores.contexts {
                let node =
                    codeyard_db_cluster::cluster::get_node_by_id(database.pool(), context.node_id)
                        .await
                        .unwrap()
                        .unwrap();
                codeyard_db_cluster::cluster::record_node_heartbeat(
                    database.pool(),
                    &context.key.public_hex(),
                    "test",
                    node.addr_hint.as_deref().unwrap(),
                    Some(free),
                )
                .await
                .unwrap();
            }
            let (status, output, errors) = stores.push(database.pool()).await;
            assert!(status.unwrap().success(), "{output:?} {errors:?}");
            let after = stores.refs().await;
            assert_eq!(after[0], after[1]);
            assert_ne!(after, before);
            stores.stop().await;
            drop(stores);
            match &original {
                Some(value) => std::env::set_var("TMPDIR", value),
                None => std::env::remove_var("TMPDIR"),
            }
            database.finish().await;
        }
    });
}

#[tokio::test]
async fn repository_creation_refuses_full_nodes_through_http_on_both_engines() {
    use crate::support::http_client::Client;
    use axum::http::StatusCode;
    use std::sync::Arc;
    for pg in [false, true] {
        let mut database = Database::empty(pg).await;
        database.complete_setup().await;
        let stores = super::quorum::Stores::new(database.pool()).await;
        database
            .execute(
                if pg {
                    "UPDATE nodes SET roles=ARRAY['web','storage']::text[] WHERE id=$1"
                } else {
                    "UPDATE nodes SET roles='[\"web\",\"storage\"]' WHERE id=$1"
                },
                &[stores.contexts[0].node_id],
            )
            .await;
        let root = tempfile::tempdir().unwrap();
        let router = codeyard_api_rest::create_router(codeyard_app::app::AppState::new(
            database.pool().clone(),
            stores.contexts[0].repos_path.clone(),
            Arc::new(
                codeyard_storage::ObjectStore::open(root.path(), None)
                    .await
                    .unwrap(),
            ),
            None,
            codeyard_ci::config::CiConfig::default(),
            Some(stores.contexts[0].clone()),
        ));
        let client = Client::login(&router, database.pool(), stores.fixture.owner).await;
        for free in [0, 1500, 10001] {
            database
                .execute("UPDATE nodes SET disk_total=10000,disk_free=$1", &[free])
                .await;
            client
                .expect(
                    "POST",
                    "/api/repos",
                    serde_json::json!({"name":"capacity-refused"}),
                    StatusCode::SERVICE_UNAVAILABLE,
                )
                .await;
            assert_eq!(
                database
                    .scalar(
                        "SELECT COUNT(*) FROM repositories WHERE name='capacity-refused'",
                        &[]
                    )
                    .await,
                0
            );
        }
        database
            .execute("UPDATE nodes SET disk_total=10000,disk_free=9000", &[])
            .await;
        client
            .expect(
                "POST",
                "/api/repos",
                serde_json::json!({"name":"capacity-restored"}),
                StatusCode::CREATED,
            )
            .await;
        assert_eq!(
            database
                .scalar(
                    "SELECT COUNT(*) FROM repositories WHERE name='capacity-restored'",
                    &[]
                )
                .await,
            1
        );
        stores.stop().await;
        database.finish().await;
    }
}
