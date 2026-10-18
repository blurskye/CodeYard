//! The dashboard's repository sections on SQLite and PostgreSQL: the viewer's own repositories,
//! repositories shared with the viewer, and Trending without the viewer's own repositories.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;

use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, repositories::lifecycle,
    storage::ObjectStore,
};
use codeyard::webui::{app::Page, pages::discovery as view};
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

/// The dashboard home page as typed data, the way the browser loads it.
async fn home(client: &Client, path: &str) -> view::HomeModel {
    let (status, page) = client.model(path).await;
    assert_eq!(status, StatusCode::OK, "{path}");
    match page {
        Page::Discovery(view::Page::Home(model)) => model,
        other => panic!("{path}: not the dashboard: {other:?}"),
    }
}

/// The filter on the dashboard narrows the viewer's own, shared and trending repositories by name
/// or description. It ignores case and the spaces around it, and echoes the text back.
async fn filter_contract(pg: bool) {
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
    let own_names = |model: &view::HomeModel| {
        let mut names: Vec<String> = model
            .repositories
            .iter()
            .map(|card| format!("{}/{}", card.owner, card.name))
            .collect();
        names.sort();
        names
    };
    let shared_names = |model: &view::HomeModel| -> Vec<String> {
        model
            .shared
            .iter()
            .map(|card| format!("{}/{}", card.repository.owner, card.repository.name))
            .collect()
    };
    let trending_names = |model: &view::HomeModel| -> Vec<String> {
        model
            .trending
            .iter()
            .map(|card| format!("{}/{}", card.owner, card.name))
            .collect()
    };

    let everything = home(&alice, "/").await;
    assert_eq!(own_names(&everything), ["alice/hidden", "alice/mine"]);
    assert_eq!(shared_names(&everything), ["bob/shared-tool"]);
    assert_eq!(trending_names(&everything), ["bob/shared-tool"]);

    let mine = home(&alice, "/?q=mine").await;
    assert_eq!(mine.find, "mine");
    assert_eq!(own_names(&mine), ["alice/mine"]);
    assert!(mine.shared.is_empty());
    assert!(trending_names(&mine).is_empty());

    // The shared repository matches by name, whatever the case, and stays in Trending.
    let tool = home(&alice, "/?q=TOOL").await;
    assert!(tool.repositories.is_empty());
    assert_eq!(shared_names(&tool), ["bob/shared-tool"]);
    assert_eq!(tool.shared[0].role, "Write");
    assert_eq!(trending_names(&tool), ["bob/shared-tool"]);

    // Surrounding spaces are not part of the filter.
    let phrase = home(&alice, "/?q=%20own%20project%20").await;
    assert_eq!(phrase.find, "own project");
    assert_eq!(own_names(&phrase), ["alice/mine"]);

    let none = home(&alice, "/?q=nothing-matches").await;
    assert!(none.repositories.is_empty());
    assert!(none.shared.is_empty());
    assert!(trending_names(&none).is_empty());
    f.finish().await;
}

#[tokio::test]
async fn sqlite_dashboard_filter_narrows_every_repository_list() {
    filter_contract(false).await;
}

#[tokio::test]
async fn postgres_dashboard_filter_narrows_every_repository_list() {
    filter_contract(true).await;
}
