//! The blame page over HTTP on disposable SQLite and PostgreSQL data: who last changed each
//! line, the blob page's link to it, and the refusals for private, unknown and binary files.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
#[path = "support/fake_webui_bundle.rs"]
mod bundle;
#[path = "support/database.rs"]
mod database;
#[path = "editor_save/fixture.rs"]
mod fixture;
#[path = "support/pull_requests.rs"]
mod http;
#[path = "support/native_database.rs"]
mod native;
#[path = "support/production_ci.rs"]
mod production;

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

/// Commits `bytes` at `path` on `main` as `author`, and returns the new tip.
#[expect(
    clippy::too_many_arguments,
    reason = "The fixture keeps publication authority explicit."
)]
async fn save(
    pool: &db::DbPool,
    repo_id: i64,
    actor: i64,
    store: &RepoStore,
    tip: &str,
    path: &str,
    bytes: &[u8],
    author: &str,
    creating: bool,
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
                message: format!("{author} changes {path}"),
                creating,
                deleting: false,
            },
            bytes,
            author,
            &format!("{}@example.invalid", author.to_lowercase()),
        )
        .await
        .unwrap()
        .sha
}

async fn blame_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("blame_owner").await;
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

    // Alice writes the file; Bob changes its second line and adds a fourth.
    let store = repo_store(&public, None);
    let first = save(
        f.pool(),
        public.id,
        owner,
        &store,
        "unborn",
        "docs/guide.txt",
        b"one\ntwo\nthree\n",
        "Alice",
        true,
    )
    .await;
    let second = save(
        f.pool(),
        public.id,
        owner,
        &store,
        &first,
        "docs/guide.txt",
        b"one\nTWO\nthree\nfour\n",
        "Bob",
        false,
    )
    .await;
    save(
        f.pool(),
        public.id,
        owner,
        &store,
        &second,
        "docs/image.bin",
        b"\0\x01\x02binary",
        "Bob",
        true,
    )
    .await;
    let secret_store = repo_store(&secret, None);
    save(
        f.pool(),
        secret.id,
        owner,
        &secret_store,
        "unborn",
        "docs/guide.txt",
        b"private\n",
        "Alice",
        true,
    )
    .await;

    let blame = "/blame_owner/public/blame/main/docs/guide.txt";
    let (status, html) = anonymous.page(blame).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    // Line 1 belongs to Alice's commit, lines 2 and 4 to Bob's, and each commit links.
    assert!(html.contains("Alice"));
    assert!(html.contains("Bob"));
    assert!(html.contains("Bob changes docs/guide.txt"));
    // A phone shows only the picture and the short sha, so the author and date are in the title.
    assert!(html.contains("title=\"Bob · "));
    assert!(html.contains("id=\"L1\""));
    assert!(html.contains("id=\"L4\""));
    assert!(html.contains("TWO"));
    assert!(html.contains(&format!("/blame_owner/public/commit/{first}")));
    assert!(html.contains(&format!("/blame_owner/public/commit/{second}")));

    // The blob page links to blame for text, and not for binary files.
    let (status, html) = anonymous
        .page("/blame_owner/public/blob/main/docs/guide.txt")
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("href=\"/blame_owner/public/blame/main/docs/guide.txt\""));
    let (status, html) = anonymous
        .page("/blame_owner/public/blob/main/docs/image.bin")
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(!html.contains("/blame/"));

    // A binary file is refused by the file's own rule, with the page's message.
    let (status, html) = anonymous
        .page("/blame_owner/public/blame/main/docs/image.bin")
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{html}");
    assert!(html.contains("Binary files cannot be blamed."));

    // Unknown paths and revisions answer the same 404 the blob page gives.
    for missing in [
        "/blame_owner/public/blame/main/docs/missing.txt",
        "/blame_owner/public/blame/nope/docs/guide.txt",
    ] {
        let (status, html) = anonymous.page(missing).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{missing}: {html}");
    }

    // A private repository answers an anonymous viewer like a missing one, and its owner reads it.
    let private = "/blame_owner/secret/blame/main/docs/guide.txt";
    let (status, html) = anonymous.page(private).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{html}");
    let (status, html) = signed_in.page(private).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("private"));

    f.finish().await;
}

#[tokio::test]
async fn sqlite_repository_blame() {
    blame_contract(false).await;
}

#[tokio::test]
async fn postgres_repository_blame() {
    blame_contract(true).await;
}
