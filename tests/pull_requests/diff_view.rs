//! The pull request files page in its unified and split views, on SQLite and PostgreSQL. The
//! address names the view with `view=split`, the toggle links to both views, and the page model
//! carries the choice the page was rendered with.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::http::StatusCode;
use base64::Engine;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, repositories::lifecycle,
    storage::ObjectStore,
};
use codeyard::webui::{app::Page, pages::repos::issues};
use database::Database;
use pulls::Client;
use serde_json::json;
use std::sync::Arc;

/// Whether the page at `path` is rendered in the split view, according to its model.
async fn split_view(client: &Client, path: &str) -> bool {
    let (status, page) = client.model(path).await;
    assert_eq!(status, StatusCode::OK, "{path}");
    match page {
        Page::Collaboration(page) => match *page {
            issues::Page::Diff { split, .. } => split,
            other => panic!("{path}: not a diff page: {other:?}"),
        },
        other => panic!("{path}: not a collaboration page: {other:?}"),
    }
}

async fn contract(pg: bool) {
    // The files page is HTML, which needs the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
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

    // The topic branch rewrites README.md, which is a change with a removed and an added line.
    let main = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic/a", &main).await;
    let content = base64::engine::general_purpose::STANDARD.encode("rewritten\n");
    let rewritten = alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/contents/README.md",
            json!({"branch": "topic/a", "content": content, "message": "Rewrite README", "expected_revision": main}),
            StatusCode::OK,
        )
        .await;
    let rewritten = rewritten["commit"]["sha"].as_str().unwrap().to_owned();
    alice.save("topic/a", "feature.txt", &rewritten).await;
    let pull = alice.open_pull("Rewrite README", "topic/a", "main").await;
    let number = pull["number"].as_i64().unwrap();
    let files = format!("/alice/proj/pulls/{number}/files");

    // Unified is the default: the page has no split table, and its toggle links to the split view.
    let (status, html) = alice.page(&files).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("rewritten"), "{html}");
    assert!(!html.contains("class=\"collaboration-split"), "{html}");
    assert!(
        html.contains(&format!(
            "href=\"/alice/proj/pulls/{number}/diff?view=split\""
        )),
        "{html}"
    );
    assert!(!split_view(&alice, &files).await);

    // The split view puts the two sides in one table; the address names it.
    let split = format!("{files}?view=split");
    let (status, html) = alice.page(&split).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("rewritten"), "{html}");
    assert!(html.contains("class=\"collaboration-split"), "{html}");
    assert!(split_view(&alice, &split).await);

    // The diff address shows the same page, and a view name that is not split is unified.
    assert!(
        split_view(
            &alice,
            &format!("/alice/proj/pulls/{number}/diff?view=split")
        )
        .await
    );
    assert!(!split_view(&alice, &format!("{files}?view=bogus")).await);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_pull_request_files_unified_and_split() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_pull_request_files_unified_and_split() {
    contract(true).await;
}
