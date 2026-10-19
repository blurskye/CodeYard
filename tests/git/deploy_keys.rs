//! Deploy keys over built-in SSH: a read-only key clones its repository and cannot push, a write
//! key pushes, a key never reaches another repository, and a deleted key stops working.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::fake_webui_bundle as bundle;
use crate::support::git_remote::{api, git, success, tool, Server};
use crate::support::native_database as native;
use chrono::{Duration as ChronoDuration, Utc};
use codeyard::backend::db;
use reqwest::{Client, StatusCode};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
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

#[tokio::test]
async fn deploy_keys_clone_push_and_stay_inside_their_repository() {
    bundle::install();
    let database = native::PostgresDatabase::new().await;
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let owner = db::users::accounts::create_user(&database.pool, "deploy_owner", None)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let session = native::token();
    db::users::sessions::create_session(
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
        db::users::sessions::get_or_create_csrf_token(
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
    let used = db::deploy_keys::get_deploy_key(&database.pool, reader_id)
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

    // Neither key reaches the other repository.
    let other = git(&work, &["ls-remote", &url("other")], None, &write_key).await;
    assert!(!other.status.success());
    let other = git(&work, &["ls-remote", &url("other")], None, &read_key).await;
    assert!(!other.status.success());

    // A rule that exempts administrators still applies to a deploy key, even though the key
    // acts for the administrator who added it.
    let project =
        db::repositories::records::get_repository(&database.pool, "deploy_owner", "project")
            .await
            .unwrap()
            .unwrap();
    db::repositories::protection::upsert_branch_protection_rule(
        &database.pool,
        db::repositories::BranchProtectionRuleUpsert {
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
}
