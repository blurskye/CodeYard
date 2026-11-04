//! Deploy keys over built-in SSH: a read-only key clones its repository and cannot push, a write
//! key pushes, a key never reaches another repository, and a deleted key stops working.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::fake_webui_bundle as bundle;
use crate::support::git_remote::{api, git, success, tool, Server};
use crate::support::native_database as native;
use chrono::{Duration as ChronoDuration, Utc};

use reqwest::{Client, StatusCode};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// A new Ed25519 key pair. Returns the private key path and the public key line.
async fn keypair(root: &Path, name: &str) -> (PathBuf, String) {
    let path = root.join(name);
    let keygen = Command::new(tool("ssh-keygen"))
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&path)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(keygen.status.success());
    let public = std::fs::read_to_string(path.with_extension("pub")).unwrap();
    (path, public)
}

async fn multiplexed_slot_wait_keeps_the_active_channel_readable(
    root: &Path,
    database: &native::PostgresDatabase,
    owner: i64,
    key: &Path,
) {
    use tokio::io::AsyncReadExt;
    let server = Server::start_with_session_limit(
        &root.join("limited"),
        database,
        "deploy-limited",
        "web,ssh",
        owner,
        Some(1),
    )
    .await;
    let socket = root.join("ssh-control");
    let ssh = || {
        let mut command = Command::new(tool("ssh"));
        command
            .args([
                "-F",
                "/dev/null",
                "-oBatchMode=yes",
                "-oIdentitiesOnly=yes",
                "-oIdentityAgent=none",
                "-oStrictHostKeyChecking=no",
                "-oUserKnownHostsFile=/dev/null",
            ])
            .arg("-S")
            .arg(&socket)
            .arg("-p")
            .arg(server.ssh_port.to_string())
            .arg("-i")
            .arg(key)
            .kill_on_drop(true);
        command
    };
    let mut master = ssh()
        .args(["-M", "-N", "git@127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        while !socket.exists() {
            assert!(master.try_wait().unwrap().is_none(), "SSH master exited");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let command = "git-upload-pack 'deploy_owner/project.git'";
    let mut first = ssh()
        .args(["-oProxyCommand=false", "git@127.0.0.1", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        first.stdout.as_mut().unwrap().read_exact(&mut [0; 4]),
    )
    .await
    .unwrap()
    .unwrap();
    let mut second = ssh()
        .args(["-v", "-oProxyCommand=false", "git@127.0.0.1", command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The master reports the session id to this client only after it has sent the second exec
    // to the server, and it keeps the connection's messages in order, so the server meets that
    // exec before the first channel's EOF.
    use tokio::io::AsyncBufReadExt;
    let mut errors = tokio::io::BufReader::new(second.stderr.take().unwrap()).lines();
    let mut stderr = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        while let Some(line) = errors.next_line().await.unwrap() {
            stderr.push_str(&line);
            stderr.push('\n');
            if line.contains("master session id") {
                return;
            }
        }
        panic!("the second exec ended before the master opened it: {stderr}");
    })
    .await
    .expect("the SSH master never opened the second exec");
    // The second exec is waiting for the first channel's slot on this same SSH connection.
    // EOF must still reach the first channel, so it can finish and release that slot.
    drop(first.stdin.take());
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        second.wait_with_output(),
    )
    .await
    .expect("waiting exec blocked the active channel's EOF")
    .unwrap();
    while let Some(line) = errors.next_line().await.unwrap() {
        stderr.push_str(&line);
        stderr.push('\n');
    }
    assert!(!output.stdout.is_empty(), "{stderr}");
    tokio::time::timeout(std::time::Duration::from_secs(120), first.wait())
        .await
        .unwrap()
        .unwrap();
    master.kill().await.unwrap();
    master.wait().await.unwrap();
}

#[tokio::test]
async fn deploy_keys_clone_push_and_stay_inside_their_repository() {
    bundle::install();
    let database = native::PostgresDatabase::new().await;
    codeyard_db_cluster::cluster::enable_cluster(&database.pool)
        .await
        .unwrap();
    codeyard_db_cluster::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let owner =
        codeyard_db_identity::users::accounts::create_user(&database.pool, "deploy_owner", None)
            .await
            .unwrap();
    let root = tempfile::tempdir().unwrap();
    let session = native::token();
    codeyard_db_identity::users::sessions::create_session(
        &database.pool,
        &session,
        owner.id,
        None,
        None,
        Utc::now() + ChronoDuration::hours(1),
    )
    .await
    .unwrap();
    let csrf = native::token();
    assert_eq!(
        codeyard_db_identity::users::sessions::get_or_create_csrf_token(
            &database.pool,
            &session,
            Some(owner.id),
            &csrf,
            3600
        )
        .await
        .unwrap(),
        Some(csrf.clone())
    );
    // Repositories live on a storage node, as in the remote Git tests.
    let _storage = Server::start(
        &root.path().join("storage"),
        &database,
        "deploy-storage",
        "web,storage",
        owner.id,
    )
    .await;
    let web = Server::start(
        &root.path().join("web"),
        &database,
        "deploy-web",
        "web,ssh",
        owner.id,
    )
    .await;
    let cookie = format!("session_id={session}");
    let client = Client::new();
    for name in ["project", "other"] {
        api(
            &client,
            &web,
            (&cookie, &csrf),
            reqwest::Method::POST,
            "/api/repos",
            json!({"name": name, "is_private": true}),
            StatusCode::CREATED,
        )
        .await;
    }

    let (write_key, write_public) = keypair(root.path(), "write").await;
    let (read_key, read_public) = keypair(root.path(), "read").await;
    let mut ids = Vec::new();
    for (title, public, can_write) in [
        ("writer", &write_public, true),
        ("reader", &read_public, false),
    ] {
        let created = api(
            &client,
            &web,
            (&cookie, &csrf),
            reqwest::Method::POST,
            "/api/repos/deploy_owner/project/keys",
            json!({"title": title, "key": public, "can_write": can_write}),
            StatusCode::CREATED,
        )
        .await;
        ids.push(created["id"].as_i64().unwrap());
    }
    let (writer_id, reader_id) = (ids[0], ids[1]);

    let url = |repo: &str| {
        format!(
            "ssh://git@127.0.0.1:{}/deploy_owner/{repo}.git",
            web.ssh_port
        )
    };
    let work = root.path().join("client");
    std::fs::create_dir(&work).unwrap();
    success(
        git(
            &work,
            &["init", "-q", "-b", "main", "source"],
            None,
            &write_key,
        )
        .await,
    );
    let source = work.join("source");
    std::fs::write(source.join("README"), "one\n").unwrap();
    success(git(&source, &["add", "README"], None, &write_key).await);
    success(git(&source, &["commit", "-q", "-m", "one"], None, &write_key).await);
    success(
        git(
            &source,
            &["push", &url("project"), "main"],
            None,
            &write_key,
        )
        .await,
    );

    // Multi-key clients do not accumulate bans, and pre-exec requests cannot deadlock exec.
    let (unknown_key, _) = keypair(root.path(), "unknown").await;
    for _ in 0..6 {
        let output = tokio::time::timeout(std::time::Duration::from_secs(120), async {
            let mut child = Command::new(tool("ssh"))
                .args([
                    "-F",
                    "/dev/null",
                    "-oBatchMode=yes",
                    "-oIdentitiesOnly=yes",
                    "-oIdentityAgent=none",
                    "-oStrictHostKeyChecking=no",
                    "-oUserKnownHostsFile=/dev/null",
                    "-oSetEnv=LANG=C GIT_PROTOCOL=version=2",
                    "-p",
                    &web.ssh_port.to_string(),
                    "-i",
                ])
                .arg(&unknown_key)
                .arg("-i")
                .arg(&read_key)
                .arg("git@127.0.0.1")
                .arg("git-upload-pack 'deploy_owner/project.git'")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let mut input = child.stdin.take().unwrap();
            // Protocol v2 advertises capabilities first; request refs explicitly.
            input
                .write_all(b"0014command=ls-refs\n00010009peel\n000csymrefs\n0000")
                .await?;
            drop(input);
            child.wait_with_output().await
        })
        .await
        .expect("environment requests deadlocked exec")
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.starts_with(b"000eversion 2\n"), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("refs/heads/main"),
            "{output:?}"
        );
    }

    multiplexed_slot_wait_keeps_the_active_channel_readable(
        root.path(),
        &database,
        owner.id,
        &read_key,
    )
    .await;

    // The read-only key clones and fetches, and its use is recorded.
    success(
        git(
            &work,
            &["clone", "-q", &url("project"), "reader"],
            None,
            &read_key,
        )
        .await,
    );
    let reader = work.join("reader");
    let used = codeyard_db_identity::deploy_keys::get_deploy_key(&database.pool, reader_id)
        .await
        .unwrap()
        .unwrap();
    assert!(used.last_used_at.is_some());

    // It cannot push, to a new branch or to main.
    std::fs::write(reader.join("README"), "two\n").unwrap();
    success(git(&reader, &["commit", "-q", "-am", "two"], None, &read_key).await);
    let refused = git(
        &reader,
        &["push", "origin", "HEAD:refs/heads/from-reader"],
        None,
        &read_key,
    )
    .await;
    assert!(!refused.status.success());

    // The write key pushes, and the read key sees the new branch.
    success(
        git(
            &reader,
            &["push", "origin", "HEAD:refs/heads/from-writer"],
            None,
            &write_key,
        )
        .await,
    );
    let listed = success(git(&reader, &["ls-remote", "origin"], None, &read_key).await);
    assert!(listed.contains("refs/heads/from-writer"), "{listed}");
    assert!(!listed.contains("refs/codeyard/"), "{listed}");

    // Neither key reaches the other repository.
    let other = git(&work, &["ls-remote", &url("other")], None, &write_key).await;
    assert!(!other.status.success());
    let other = git(&work, &["ls-remote", &url("other")], None, &read_key).await;
    assert!(!other.status.success());

    // A rule that exempts administrators still applies to a deploy key, even though the key
    // acts for the administrator who added it.
    let project = codeyard_db_repos::repositories::records::get_repository(
        &database.pool,
        "deploy_owner",
        "project",
    )
    .await
    .unwrap()
    .unwrap();
    let physical = root.path().join("storage/repos").join(format!(
        "{}.git",
        codeyard::backend::cluster::rpc::repository_uuid(&project.disk_path).unwrap()
    ));
    let provenance = codeyard::backend::repositories::store::RepoStore::from_disk_path(
        &physical.to_string_lossy(),
    )
    .branch_publication(project.id, "refs/heads/from-writer")
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        provenance.publication.actor,
        codeyard_git::PublicationActor::DeployKey(writer_id)
    );
    assert_eq!(provenance.publication.actor.user_id(), None);
    codeyard_db_repos::repositories::protection::upsert_branch_protection_rule(
        &database.pool,
        codeyard_db_repos::repositories::BranchProtectionRuleUpsert {
            actor_id: owner.id,
            repo_id: project.id,
            pattern: "guarded",
            require_pull_request: true,
            enforce_admins: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let guarded = git(
        &reader,
        &["push", "origin", "HEAD:refs/heads/guarded"],
        None,
        &write_key,
    )
    .await;
    assert!(!guarded.status.success());

    api(
        &client,
        &web,
        (&cookie, &csrf),
        reqwest::Method::POST,
        "/api/repos/deploy_owner/project/pulls",
        json!({"title": "Deploy key PR", "head_branch": "from-writer", "base_branch": "main"}),
        StatusCode::CREATED,
    )
    .await;

    // An owner-created write key cannot acquire deployment rights through YAML promotion.
    std::fs::create_dir_all(reader.join(".github/workflows")).unwrap();
    std::fs::write(reader.join(".github/workflows/ci.yml"),
        "on: [push, pull_request]\njobs:\n  deploy:\n    runs-on: ubuntu-latest\n    environment: production\n    steps: []\n").unwrap();
    success(git(&reader, &["add", ".github"], None, &write_key).await);
    success(
        git(
            &reader,
            &["commit", "-q", "-m", "deploy workflow"],
            None,
            &write_key,
        )
        .await,
    );
    let deployment_sha = success(git(&reader, &["rev-parse", "HEAD"], None, &write_key).await);
    let _ = git(
        &reader,
        &["push", "origin", "HEAD:refs/heads/deploy-env"],
        None,
        &write_key,
    )
    .await;
    let listed = success(
        git(
            &reader,
            &["ls-remote", "origin", "refs/heads/deploy-env"],
            None,
            &read_key,
        )
        .await,
    );
    assert!(
        listed.starts_with(&deployment_sha),
        "the Git write itself was accepted"
    );
    assert!(
        codeyard_db_actions::ci::list_ci_runs_for_repo(&database.pool, project.id, 100)
            .await
            .unwrap()
            .iter()
            .all(|run| run.head_sha.as_deref() != Some(deployment_sha.as_str()))
    );

    // Refreshing an open PR must retain the deploy credential's Write cap too.
    // Push workflow admission also refuses the deployment and can report a partial failure.
    // Check the accepted Git ref and PR refresh independently of that follow-up status.
    let _ = git(
        &reader,
        &["push", "origin", "HEAD:refs/heads/from-writer"],
        None,
        &write_key,
    )
    .await;
    let listed = success(
        git(
            &reader,
            &["ls-remote", "origin", "refs/heads/from-writer"],
            None,
            &read_key,
        )
        .await,
    );
    assert!(listed.starts_with(&deployment_sha));
    let pull =
        codeyard_db_repos::pull_requests::records::get_pull_request(&database.pool, project.id, 1)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(pull.head_sha, deployment_sha);
    assert!(
        codeyard_db_actions::ci::list_ci_runs_for_repo(&database.pool, project.id, 100)
            .await
            .unwrap()
            .iter()
            .all(|run| run.head_sha.as_deref() != Some(deployment_sha.as_str()))
    );

    // A deleted key stops working at once.
    let deleted = client
        .delete(format!(
            "{}/api/repos/deploy_owner/project/keys/{writer_id}",
            web.base
        ))
        .header("Cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let revoked = git(
        &reader,
        &["push", "origin", "HEAD:refs/heads/after-delete"],
        None,
        &write_key,
    )
    .await;
    assert!(!revoked.status.success());
    let still_reads = git(&reader, &["fetch", "-q", "origin"], None, &read_key).await;
    assert!(still_reads.status.success());

    let observed = codeyard::backend::repositories::traffic::summary(
        &database.pool,
        Some(&owner),
        &project,
        codeyard::backend::repositories::traffic::Kind::Clones,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        observed.totals.count, 1,
        "SSH advertisements, slot waits, failed reads and fetches are not clones"
    );
    assert_eq!(observed.totals.uniques, 1);
    success(
        git(
            &work,
            &["clone", "-q", &url("project"), "reader-again"],
            None,
            &read_key,
        )
        .await,
    );
    let observed = codeyard::backend::repositories::traffic::summary(
        &database.pool,
        Some(&owner),
        &project,
        codeyard::backend::repositories::traffic::Kind::Clones,
        false,
    )
    .await
    .unwrap();
    assert_eq!(observed.totals.count, 2);
    assert_eq!(
        observed.totals.uniques, 1,
        "The same deploy key is one visitor across connections"
    );
}
