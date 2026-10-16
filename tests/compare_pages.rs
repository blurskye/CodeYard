//! The branch comparison page over HTTP: a public repository reads anonymously, a private one a
//! stranger cannot read stays a plain 404, the same branch and an unknown branch have their own
//! answers, the form's address redirects to the one that names both branches, and a comparison
//! with changes offers a pull request. On SQLite and PostgreSQL.
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
    api::create_router, app::AppState, ci::CiConfig, db, repositories::lifecycle::provision,
    storage::ObjectStore,
};
use database::Database;
use pulls::Client;
use std::sync::Arc;

async fn contract(pg: bool) {
    // The answers below are HTML pages, which need the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    for (name, private) in [("proj", false), ("secret", true)] {
        provision(
            (f.pool(), None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
            name,
            None,
            private,
            None,
        )
        .await
        .unwrap();
    }
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let anonymous = Client::anonymous(&router);
    // `topic/a` adds one file to `main`.
    let main = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic/a", &main).await;
    alice.save("topic/a", "feature.txt", &main).await;

    // Anyone who may read the repository sees the changes, signed in or not.
    let (status, html) = anonymous.page("/alice/proj/compare/main...topic/a").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Comparing changes"), "{html}");
    assert!(html.contains("repo-tabs"), "{html}");
    assert!(html.contains("feature.txt"), "{html}");
    // The changes lead to the pull request form, prefilled with both branches.
    assert!(html.contains("Create pull request"), "{html}");
    assert!(html.contains("/alice/proj/pulls/new?base=main"), "{html}");
    assert!(html.contains("head=topic%2Fa"), "{html}");
    // The same branch on both sides shows the form with its error, not a refused comparison.
    let (status, form) = alice
        .page("/alice/proj/pulls/new?base=main&head=main")
        .await;
    assert_eq!(status, StatusCode::OK, "{form}");
    assert!(
        form.contains("Select two different, existing branches."),
        "{form}"
    );

    // A private repository that a stranger cannot read is refused exactly like a missing one.
    for viewer in [&bob, &anonymous] {
        let (status, html) = viewer.page("/alice/secret/compare/main...main").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{html}");
        assert!(!html.contains("repo-tabs"), "{html}");
    }

    // The same branch on both sides has nothing to compare and no pull request to open.
    let (status, html) = anonymous.page("/alice/proj/compare/main...main").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    // The apostrophe is HTML-escaped in the page, so the test matches the rest of the sentence.
    assert!(html.contains("anything to compare."), "{html}");
    assert!(!html.contains("Create pull request"), "{html}");

    // A branch that does not exist is a 404 in the repository's own page.
    let (status, html) = anonymous.page("/alice/proj/compare/main...nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{html}");
    assert!(html.contains("Branch not found."), "{html}");
    assert!(html.contains("repo-tabs"), "{html}");

    // The form sends `?base=&head=`; the address that names both branches is the canonical one.
    // A field left empty means the default branch.
    let moved = anonymous
        .exchange(
            "GET",
            "/alice/proj/compare?base=main&head=topic%2Fa",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(moved.status, StatusCode::SEE_OTHER);
    assert_eq!(
        moved.location.as_deref(),
        Some("/alice/proj/compare/main...topic/a")
    );
    let moved = anonymous
        .exchange(
            "GET",
            "/alice/proj/compare?head=topic%2Fa",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(
        moved.location.as_deref(),
        Some("/alice/proj/compare/main...topic/a")
    );

    // The bare address compares the default branch with itself.
    let (status, html) = anonymous.page("/alice/proj/compare").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("anything to compare."), "{html}");

    // The branch list links each branch to its comparison with the default branch.
    let (status, html) = anonymous.page("/alice/proj/branches").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("/alice/proj/compare/main...topic/a"),
        "{html}"
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_compare_pages_follow_repository_read_access() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_compare_pages_follow_repository_read_access() {
    contract(true).await;
}
