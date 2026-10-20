//! The "Go to file" page over HTTP on disposable SQLite and PostgreSQL data: the file listing of
//! a branch, the `q` filter, the repository root's link to it, and the refusals for private,
//! unknown and nested revisions.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as http;

use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db,
    repositories::{
        lifecycle::provision,
        store::{repo_store, FileWrite, RepoStore},
    },
    storage::ObjectStore,
};
use database::Database;
use std::sync::Arc;

/// Commits `bytes` as a new file at `path` on `main`, after `tip`, and returns the new tip.
async fn save(
    pool: &db::DbPool,
    repo_id: i64,
    actor: i64,
    store: &RepoStore,
    tip: &str,
    path: &str,
    bytes: &[u8],
) -> String {
    store
        .save_file_as(
            pool,
            repo_id,
            actor,
            &FileWrite {
                branch: "main".into(),
                path: path.into(),
                expected_revision: tip.into(),
                message: format!("Add {path}"),
                creating: true,
                deleting: false,
            },
            bytes,
            "Alice",
            "alice@example.invalid",
        )
        .await
        .unwrap()
        .sha
}

async fn find_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("find_owner").await;
    let root = tempfile::tempdir().unwrap();
    let public = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "public",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let secret = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "secret",
        None,
        true,
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
    let anonymous = http::Client::anonymous(&router);
    let signed_in = http::Client::login(&router, f.pool(), owner).await;

    let store = repo_store(&public, None);
    let mut tip = "unborn".to_owned();
    for path in [
        "README.md",
        "src/lib.rs",
        "docs/Guide.md",
        "docs/guide-old.txt",
    ] {
        tip = save(
            f.pool(),
            public.id,
            owner,
            &store,
            &tip,
            path,
            path.as_bytes(),
        )
        .await;
    }
    let secret_store = repo_store(&secret, None);
    save(
        f.pool(),
        secret.id,
        owner,
        &secret_store,
        "unborn",
        "private.txt",
        b"private\n",
    )
    .await;

    // Every path of the branch is listed, each linked to its file.
    let (status, html) = anonymous.page("/find_owner/public/find/main").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    for path in [
        "README.md",
        "src/lib.rs",
        "docs/Guide.md",
        "docs/guide-old.txt",
    ] {
        assert!(
            html.contains(&format!(
                "href=\"/find_owner/public/blob/refs/heads/main/{path}\""
            )),
            "{path}: {html}"
        );
    }
    assert!(!html.contains("class=\"repo-find-first\""));

    // The query keeps the paths that contain every term, ignoring case, and marks the first.
    let (status, html) = anonymous
        .page("/find_owner/public/find/main?q=GUIDE%20md")
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("href=\"/find_owner/public/blob/refs/heads/main/docs/Guide.md\""));
    assert!(html.contains("1 file<"), "{html}");
    assert!(html.contains("Go to file on"), "{html}");
    assert!(!html.contains("/find_owner/public/blob/refs/heads/main/docs/guide-old.txt"));
    assert!(!html.contains("/find_owner/public/blob/refs/heads/main/README.md"));
    assert!(html.contains("class=\"repo-find-first\""));
    let (status, html) = anonymous
        .page("/find_owner/public/find/main?q=nothing-here")
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("0 files"), "{html}");
    assert!(html.contains("No files match"), "{html}");

    let (status, model) = anonymous
        .model(&format!(
            "/find_owner/public/find/main?q={}",
            "a%20".repeat(2000)
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let codeyard::webui::app::Page::Repositories(codeyard::webui::pages::repos::page::Page::Find {
        page,
        ..
    }) = model
    else {
        panic!("find page")
    };
    assert_eq!(page.query.chars().count(), 256);

    // A private repository answers an anonymous viewer like a missing one, and its owner reads it.
    let (status, html) = anonymous.page("/find_owner/secret/find/main").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{html}");
    let (status, html) = signed_in.page("/find_owner/secret/find/main").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("private.txt"));

    // An unknown branch, and a path after the revision, are not pages.
    for missing in [
        "/find_owner/public/find/nope",
        "/find_owner/public/find/main/src",
    ] {
        let (status, html) = anonymous.page(missing).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{missing}: {html}");
    }

    // The repository root links to Go to file and counts its branches and tags.
    let (status, html) = anonymous.page("/find_owner/public").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("action=\"/find_owner/public/find/main\""));
    assert!(html.contains("1 branch"));
    assert!(html.contains("0 tags"));

    f.finish().await;
}

#[tokio::test]
async fn sqlite_repository_find() {
    find_contract(false).await;
}

#[tokio::test]
async fn postgres_repository_find() {
    find_contract(true).await;
}
