//! Real receive-pack rejects quarantined secrets and honors only the pusher's bypass.
use crate::support::database::Database;
use crate::support::git_server;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_app::ci::config::CiConfig;
use codeyard_app::repositories::lifecycle;
use codeyard_app::repositories::store::repo_store;
use codeyard_storage::ObjectStore;
use codeyard_db_security::secret_scanning as records;
use codeyard_support::secrets::SecretEncryptor;
use serde_json::{json, Value};
use std::{path::Path, process::Output, sync::Arc};

async fn git(root: &Path, args: &[&str], token: Option<&str>) -> Output {
    let root = root.to_owned();
    let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let token = token.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        git_server::git(
            &root,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            token.as_deref(),
        )
    })
    .await
    .unwrap()
}
async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut fixture = Database::new(pg).await;
    fixture.complete_setup().await;
    let owner = fixture.actor("scanner-pusher").await;
    let root = tempfile::tempdir().unwrap();
    let repos = root.path().join("repos");
    let repo = lifecycle::provision(
        (fixture.pool(), None),
        &repos,
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner),
        "protected",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let state = AppState::new(
        fixture.pool().clone(),
        repos,
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(Arc::new(SecretEncryptor::new(&[42; 32]))),
        CiConfig::default(),
        None,
    );
    let router = create_router(state);
    let session = Client::login(&router, fixture.pool(), owner).await;
    let token = git_server::write_token(fixture.pool(), owner).await;
    let (status, _) = session
        .form(
            "/scanner-pusher/protected/_forms/code-security",
            &[
                ("repository_id", &repo.id.to_string()),
                ("secret_scanning", "on"),
                ("push_protection", "on"),
            ],
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::SEE_OTHER);
    let server = git_server::Server::start_with_master_key(
        root.path(),
        &fixture.connection_url(),
        &"2a".repeat(32),
    )
    .await;
    let base = &server.base;
    let local = root.path().join("client");
    std::fs::create_dir(&local).unwrap();
    git_server::success(git(&local, &["init", "-b", "main"], None).await);
    std::fs::write(local.join("README.md"), "Safe content\n").unwrap();
    git_server::success(git(&local, &["add", "."], None).await);
    git_server::success(git(&local, &["commit", "-m", "initial"], None).await);
    let remote = format!("{base}/scanner-pusher/protected.git");
    git_server::success(git(&local, &["remote", "add", "origin", &remote], None).await);
    git_server::success(git(&local, &["push", "origin", "main"], Some(&token)).await);
    let old = repo_store(&repo, None).commit("main").await.unwrap().sha;
    let secret = format!(
        "cy_pat_{}",
        "3b17a095fdc84e62091adc64b3f5a7e891c246df938b5a017eafc647320bd952"
    );
    std::fs::write(local.join("credential.env"), format!("TOKEN={secret}\n")).unwrap();
    git_server::success(git(&local, &["add", "."], None).await);
    git_server::success(git(&local, &["commit", "-m", "credential"], None).await);
    // A later mode-only change must not hide the commit that introduced the credential.
    git_server::success(
        git(
            &local,
            &["update-index", "--chmod=+x", "credential.env"],
            None,
        )
        .await,
    );
    git_server::success(git(&local, &["commit", "-m", "mode change"], None).await);
    let output = git(&local, &["push", "origin", "main"], Some(&token)).await;
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("GH013"), "{message}");
    assert!(message.contains("codeyard-pat"), "{message}");
    assert!(message.contains("credential.env:1"), "{message}");
    assert!(
        !message.contains(&secret),
        "The rejection must not disclose credential values"
    );
    assert_eq!(
        repo_store(&repo, None).commit("main").await.unwrap().sha,
        old
    );
    let id = message
        .split("/unblock-secret/")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    let page = session
        .page_ok(&format!(
            "/scanner-pusher/protected/security/secret-scanning/unblock-secret/{id}"
        ))
        .await;
    assert!(page.contains("used_in_tests"));
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "{base}/api/v3/repos/scanner-pusher/protected/secret-scanning/push-protection-bypasses"
        ))
        .bearer_auth(&token)
        .json(&json!({"reason":"used_in_tests","placeholder_id":id}))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_github_schema(
        "secret-scanning/create-push-protection-bypass",
        status,
        &body,
    );
    git_server::success(git(&local, &["push", "origin", "main"], Some(&token)).await);
    let mut body = Value::Null;
    for _ in 0..50 {
        let response = client
            .get(format!(
                "{base}/api/v3/repos/scanner-pusher/protected/secret-scanning/alerts"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        body = response.json().await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert_github_schema("secret-scanning/list-alerts-for-repo", status, &body);
        if body.as_array().is_some_and(|alerts| !alerts.is_empty()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert_eq!(body.as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body[0]["push_protection_bypassed"], true);
    assert_eq!(body[0]["resolution"], "used_in_tests");
    assert_eq!(
        body[0]["push_protection_bypassed_by"]["login"],
        "scanner-pusher"
    );
    // A clean push is still accepted after the bypass expires; existing values are not newly added.
    fixture.execute("UPDATE secret_scanning_bypasses SET expires_at='2000-01-01T00:00:00Z' WHERE repo_id=$1",&[repo.id]).await;
    std::fs::write(local.join("README.md"), "Safe change\n").unwrap();
    git_server::success(git(&local, &["add", "."], None).await);
    git_server::success(git(&local, &["commit", "-m", "clean follow-up"], None).await);
    git_server::success(git(&local, &["push", "origin", "main"], Some(&token)).await);
    let placeholder = records::protection(
        fixture.pool(),
        owner,
        repo.id,
        &codeyard_support::token_hash::token_hash(&secret),
        "codeyard_personal_access_token",
    )
    .await
    .unwrap();
    assert!(placeholder.is_some());
    // A secret in a skipped binary file was never found, so it is new once the file is text.
    let hidden = format!(
        "cy_pat_{}",
        "8d2f61c04b9e37a5d0c8e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6"
    );
    std::fs::write(local.join("hidden.env"), format!("TOKEN={hidden}\n\0")).unwrap();
    git_server::success(git(&local, &["add", "."], None).await);
    git_server::success(git(&local, &["commit", "-m", "binary"], None).await);
    std::fs::write(local.join("hidden.env"), format!("TOKEN={hidden}\n")).unwrap();
    git_server::success(git(&local, &["add", "."], None).await);
    git_server::success(git(&local, &["commit", "-m", "text"], None).await);
    let output = git(&local, &["push", "origin", "main"], Some(&token)).await;
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("hidden.env:1"), "{message}");
    drop(server);
    fixture.finish().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credential_push_protection_sqlite() {
    contract(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credential_push_protection_postgres() {
    contract(true).await;
}
