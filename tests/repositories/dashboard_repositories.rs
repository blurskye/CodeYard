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

async fn bounded_repository_pages(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("page_owner").await;
    let viewer = f.actor("page_viewer").await;
    f.execute("WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<35) INSERT INTO repositories(owner_kind,owner_user_id,name,is_private,disk_path) SELECT 'user',$1,'bulk-' || n,FALSE,'fixture' FROM numbers", &[owner]).await;
    f.execute("INSERT INTO repositories(owner_kind,owner_user_id,name,is_private,disk_path) VALUES('user',$1,'secret',TRUE,'fixture-secret')", &[owner]).await;
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), owner).await;
    let bob = Client::login(&router, f.pool(), viewer).await;
    let first = home(&alice, "/").await;
    let second = home(&alice, "/?page=2").await;
    assert_eq!(
        (first.repositories.len(), second.repositories.len()),
        (30, 6)
    );
    assert_eq!(first.repository_count, 36);
    assert!(first.has_older && !second.has_older);
    assert!(first
        .repositories
        .iter()
        .all(|a| second.repositories.iter().all(|b| a.name != b.name)));
    let filtered = home(&alice, "/?q=bulk-35").await;
    assert_eq!(
        (filtered.repositories.len(), filtered.repository_count),
        (1, 1)
    );
    for (client, total, tail) in [(&bob, 35, 5), (&alice, 36, 6)] {
        let (status, page) = client.model("/page_owner?repo_page=2").await;
        assert_eq!(status, StatusCode::OK);
        let Page::Identity(codeyard::webui::pages::account::Page::Profile {
            repositories,
            repository_count,
            repositories_newer,
            repositories_older,
            ..
        }) = page
        else {
            panic!("profile")
        };
        assert_eq!(repository_count, total);
        assert_eq!(repositories.len(), tail);
        assert!(repositories_newer.is_some() && repositories_older.is_none());
        let response = client
            .expect(
                "GET",
                "/api/users/page_owner/repos?page=2",
                serde_json::Value::Null,
                StatusCode::OK,
            )
            .await;
        assert_eq!(response.as_array().unwrap().len(), tail);
    }
    let (status, html) = bob.page("/page_owner").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("repo_page=2"));
    assert!(!html.contains("/page_owner/secret"));
    f.finish().await;
}
#[tokio::test]
async fn sqlite_home_profile_and_api_repository_pages_are_bounded() {
    bounded_repository_pages(false).await;
}
#[tokio::test]
async fn postgres_home_profile_and_api_repository_pages_are_bounded() {
    bounded_repository_pages(true).await;
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
