//! A base maintainer pushes over built-in SSH to the head branch of a fork's pull request that
//! allows maintainer edits, and to nothing else in that fork.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::fake_webui_bundle as bundle;
use crate::support::git_remote::{git, tool, Server};
use crate::support::native_database as native;
use chrono::{Duration as ChronoDuration, Utc};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// An account with an SSH key and a write token.
async fn account(
    pool: &codeyard_db_core::DbPool,
    root: &Path,
    name: &str,
) -> (codeyard_db_core::models::User, PathBuf, String) {
    let user = codeyard_db_identity::users::accounts::create_user(pool, name, None)
        .await
        .unwrap();
    let key = root.join(format!("{name}-key"));
    let keygen = Command::new(tool("ssh-keygen"))
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(keygen.status.success());
    codeyard_db_identity::users::ssh_keys::add_ssh_key(
        pool,
        user.id,
        name,
        &std::fs::read_to_string(key.with_extension("pub")).unwrap(),
    )
    .await
    .unwrap();
    let session = native::token();
    codeyard_db_identity::users::sessions::create_session(
        pool,
        &session,
        user.id,
        None,
        None,
        Utc::now() + ChronoDuration::hours(1),
    )
    .await
    .unwrap();
    let token = native::token();
    codeyard_db_identity::users::tokens::create_token(
        pool,
        user.id,
        &session,
        "write",
        &hex::encode(Sha256::digest(token.as_bytes())),
        "write",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    (user, key, token)
}

async fn call(
    client: &Client,
    server: &Server,
    token: &str,
    path: &str,
    body: Value,
    status: StatusCode,
) -> Value {
    let response = client
        .post(format!("{}{path}", server.base))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let actual = response.status();
    let text = response.text().await.unwrap();
    assert_eq!(actual, status, "{path}: {text}");
    serde_json::from_str(&text).unwrap()
}

/// Runs Git and, when it fails, shows the server log next to Git's error.
async fn run(server: &Server, directory: &Path, args: &[&str], key: &Path) -> String {
    let output = git(directory, args, None, key).await;
    assert!(
        output.status.success(),
        "git {args:?}: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(&server.log).unwrap_or_default()
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn base_maintainers_push_a_fork_pull_head_over_ssh() {
    bundle::install();
    let database = native::PostgresDatabase::new().await;
    native::enable_cluster(&database.pool).await.unwrap();
    codeyard_db_cluster::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let (alice, alice_key, alice_token) = account(&database.pool, root.path(), "upstream").await;
    let (_, bob_key, bob_token) = account(&database.pool, root.path(), "forker").await;
    let _storage = Server::start(
        &root.path().join("storage"),
        &database,
        "maintainer-storage",
        "web,storage",
        alice.id,
    )
    .await;
    let web = Server::start(
        &root.path().join("web"),
        &database,
        "maintainer-web",
        "web,ssh",
        alice.id,
    )
    .await;
    let client = Client::new();
    call(
        &client,
        &web,
        &alice_token,
        "/api/v3/user/repos",
        json!({"name":"project"}),
        StatusCode::CREATED,
    )
    .await;
    let url = |owner: &str| format!("ssh://git@127.0.0.1:{}/{owner}/project.git", web.ssh_port);
    let work = root.path().join("client");
    std::fs::create_dir(&work).unwrap();
    run(
        &web,
        &work,
        &["init", "-q", "-b", "main", "base"],
        &alice_key,
    )
    .await;
    let base = work.join("base");
    std::fs::write(base.join("README"), "base\n").unwrap();
    run(&web, &base, &["add", "README"], &alice_key).await;
    run(&web, &base, &["commit", "-q", "-m", "base"], &alice_key).await;
    run(&web, &base, &["push", &url("upstream"), "main"], &alice_key).await;
    call(
        &client,
        &web,
        &bob_token,
        "/api/v3/repos/upstream/project/forks",
        json!({}),
        StatusCode::ACCEPTED,
    )
    .await;
    run(&web, &base, &["checkout", "-q", "-b", "topic"], &bob_key).await;
    std::fs::write(base.join("topic.txt"), "fork\n").unwrap();
    run(&web, &base, &["add", "topic.txt"], &bob_key).await;
    run(&web, &base, &["commit", "-q", "-m", "topic"], &bob_key).await;
    run(&web, &base, &["push", &url("forker"), "topic"], &bob_key).await;
    call(
        &client,
        &web,
        &bob_token,
        "/api/v3/repos/upstream/project/pulls",
        json!({"title":"Topic","head":"forker:topic","base":"main","maintainer_can_modify":true}),
        StatusCode::CREATED,
    )
    .await;
    std::fs::write(base.join("topic.txt"), "maintainer\n").unwrap();
    run(
        &web,
        &base,
        &["commit", "-q", "-am", "maintainer"],
        &alice_key,
    )
    .await;
    run(&web, &base, &["push", &url("forker"), "topic"], &alice_key).await;
    // The grant covers the pull head only.
    let refused = git(
        &base,
        &["push", &url("forker"), "topic:main"],
        None,
        &alice_key,
    )
    .await;
    assert!(!refused.status.success());
    let listed = run(&web, &base, &["ls-remote", &url("forker")], &alice_key).await;
    let tip = run(&web, &base, &["rev-parse", "HEAD"], &alice_key).await;
    assert!(
        listed.contains(&format!("{tip}\trefs/heads/topic")),
        "{listed}"
    );
    assert!(
        !listed.contains(&format!("{tip}\trefs/heads/main")),
        "{listed}"
    );
}
