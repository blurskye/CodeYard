//! The dashboard's repository sections on SQLite and PostgreSQL: the viewer's own repositories,
//! repositories shared with the viewer, and Trending without the viewer's own repositories.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
#[path = "support/fake_webui_bundle.rs"]
mod bundle;
#[path = "support/database.rs"]
mod database;
#[path = "editor_save/fixture.rs"]
mod fixture;
#[path = "support/native_database.rs"]
mod native;
#[path = "support/production_ci.rs"]
mod production;
#[path = "support/pull_requests.rs"]
mod pulls;

use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, repositories::lifecycle,
    storage::ObjectStore,
};
use database::Database;
use pulls::Client;
use std::path::Path;
use std::sync::Arc;

async fn provision(
    pool: &db::DbPool,
    root: &Path,
    owner: i64,
    name: &str,
    description: Option<&str>,
    private: bool,
) -> db::models::Repository {
    lifecycle::provision(
        (pool, None),
        root,
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        name,
        description,
        private,
        None,
    )
    .await
    .unwrap()
}

async fn contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    provision(
        f.pool(),
        root.path(),
        alice_id,
        "mine",
        Some("Alice's own project"),
        false,
    )
    .await;
    provision(f.pool(), root.path(), alice_id, "hidden", None, true).await;
    let shared = provision(
        f.pool(),
        root.path(),
        bob_id,
        "shared-tool",
        Some("Shared tooling"),
        false,
    )
    .await;
    provision(
        f.pool(),
        root.path(),
        bob_id,
        "bobs-own",
        Some("Bob's own project"),
        false,
    )
    .await;
    db::repositories::access::grant_repo_access(
        f.pool(),
        alice_id,
        shared.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;

    let (status, html) = alice.page("/").await;
    assert_eq!(status, StatusCode::OK);
    let sidebar = html.find("<aside").unwrap();
    let (main, trending) = html.split_at(sidebar);
    // Your repositories lists the viewer's own repositories, public and private.
    assert!(main.contains("href=\"/alice/mine\""), "{html}");
    assert!(main.contains("href=\"/alice/hidden\""), "{html}");
    // Shared with you lists the collaborator repository with its role, and nothing else.
    assert!(main.contains("Shared with you"), "{html}");
    assert!(main.contains("href=\"/bob/shared-tool\""), "{html}");
    assert!(main.contains("Write"), "{html}");
    assert!(!main.contains("href=\"/bob/bobs-own\""), "{html}");
    // Trending leaves out the viewer's own repositories and keeps other people's.
    assert!(!trending.contains("href=\"/alice/mine\""), "{html}");
    assert!(!trending.contains("href=\"/alice/hidden\""), "{html}");
    assert!(trending.contains("href=\"/bob/bobs-own\""), "{html}");
    assert!(trending.contains("href=\"/bob/shared-tool\""), "{html}");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_dashboard_repository_sections() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_dashboard_repository_sections() {
    contract(true).await;
}
