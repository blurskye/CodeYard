//! Archiving a repository makes it read-only for everyone, and unarchiving gives that back. The
//! default branch is chosen among existing branches by an administrator. On SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client::Client;
use axum::http::StatusCode;
use codeyard::backend::{db, db::models::AccessLevel};
use database::Database;
use serde_json::{json, Value};

async fn contract(pg: bool) {
    // The native forms answer with a rendered page, which needs the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let proj = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap()
    .id;
    db::repositories::access::grant_repo_access(f.pool(), bob_id, proj, AccessLevel::Write)
        .await
        .unwrap();
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let repo = proj.to_string();
    let settings = "/alice/proj/_forms";

    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic", &base).await;
    alice.save("topic", "topic.txt", &base).await;

    // Only an administrator picks the default branch, and only among existing branches.
    let (status, page) = bob
        .form(
            &format!("{settings}/default-branch"),
            &[("repository_id", repo.as_str()), ("branch", "topic")],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let reply = alice
        .form_reply(
            &format!("{settings}/default-branch"),
            &[("repository_id", repo.as_str()), ("branch", "topic")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let branches = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/branches",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let default = branches
        .as_array()
        .unwrap()
        .iter()
        .find(|branch| branch["is_default"] == true)
        .expect("a default branch");
    assert_eq!(default["name"], "topic", "{branches}");
    let missing = alice
        .form_reply(
            &format!("{settings}/default-branch"),
            &[("repository_id", repo.as_str()), ("branch", "nope")],
        )
        .await;
    assert!(missing.status.is_client_error(), "{}", missing.text());

    // Archiving needs the confirmation and an administrator. Archived, the repository refuses
    // changes from everyone, and says so on its pages.
    let (status, page) = bob
        .form(
            &format!("{settings}/archive"),
            &[("repository_id", repo.as_str()), ("confirm", "yes")],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = alice
        .form(
            &format!("{settings}/archive"),
            &[("repository_id", repo.as_str())],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    let archived = alice
        .form_reply(
            &format!("{settings}/archive"),
            &[("repository_id", repo.as_str()), ("confirm", "yes")],
        )
        .await;
    assert_eq!(
        archived.status,
        StatusCode::SEE_OTHER,
        "{}",
        archived.text()
    );
    let (_, page) = bob.page("/alice/proj").await;
    assert!(
        page.contains("This repository is archived and read-only."),
        "{page}"
    );
    bob.expect(
        "POST",
        "/api/repos/alice/proj/issues",
        json!({"title": "Blocked", "body": ""}),
        StatusCode::FORBIDDEN,
    )
    .await;
    let refused = alice
        .form_reply(
            &format!("{settings}/default-branch"),
            &[("repository_id", repo.as_str()), ("branch", "main")],
        )
        .await;
    assert!(refused.status.is_client_error(), "{}", refused.text());

    // Unarchiving gives the repository back to its writers.
    let unarchived = alice
        .form_reply(
            &format!("{settings}/unarchive"),
            &[("repository_id", repo.as_str()), ("confirm", "yes")],
        )
        .await;
    assert_eq!(
        unarchived.status,
        StatusCode::SEE_OTHER,
        "{}",
        unarchived.text()
    );
    let page = bob.page_ok("/alice/proj").await;
    assert!(
        !page.contains("This repository is archived and read-only."),
        "{page}"
    );
    bob.expect(
        "POST",
        "/api/repos/alice/proj/issues",
        json!({"title": "Allowed", "body": ""}),
        StatusCode::CREATED,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn postgres_archive_and_default_branch() {
    contract(true).await;
}

#[tokio::test]
async fn sqlite_archive_and_default_branch() {
    contract(false).await;
}
