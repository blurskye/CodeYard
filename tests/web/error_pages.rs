//! Error pages over HTTP: repository errors keep the repository header for a viewer who may
//! read it, a private repository stays a plain 404, and a refused repository name is marked
//! beside its field. On SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client as pulls;

use axum::http::StatusCode;
use codeyard::backend::{db, repositories::lifecycle::provision};
use database::Database;
use pulls::Client;

async fn contract(pg: bool) {
    // The answers below are HTML pages, which need the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
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
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let anonymous = Client::anonymous(&router);
    alice.save("main", "README.md", "unborn").await;

    // A missing file in a repository the viewer can read keeps the repository header.
    let (status, html) = bob.page("/alice/proj/blob/main/nope.rs").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("Page not found"), "{html}");
    assert!(html.contains("repo-tabs"), "{html}");
    assert!(html.contains("Go to repository"), "{html}");
    assert!(
        html.contains("Check the path or revision, or pick another branch, tag or commit."),
        "{html}"
    );

    // Repository settings refuse a member on the repository's own page, in plain words.
    let (status, html) = bob.page("/alice/proj/settings").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(html.contains("Access denied"), "{html}");
    assert!(html.contains("repo-tabs"), "{html}");
    assert!(
        html.contains("Only repository administrators can manage these settings."),
        "{html}"
    );

    // A private repository that bob cannot read is refused exactly like a missing one.
    let (status, html) = bob.page("/alice/secret").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!html.contains("repo-tabs"), "{html}");
    assert!(!html.contains("Go to repository"), "{html}");

    // A missing page gets a plain message and a way forward.
    let (status, html) = anonymous.page("/no-such-page-here").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("<h1>Page not found</h1>"), "{html}");
    assert!(
        html.contains("<a href=\"/login\" class=\"btn btn-primary\">Sign in</a>"),
        "{html}"
    );

    // An address typed with a trailing slash goes to the one without it, keeping the query.
    // Only reads are redirected, and a doubled slash never becomes a redirect to another host.
    let moved = anonymous
        .exchange(
            "GET",
            "/alice/proj/issues/?state=closed",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(moved.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        moved.location.as_deref(),
        Some("/alice/proj/issues?state=closed")
    );
    let posted = anonymous
        .exchange("POST", "/alice/proj/issues/", "text/html", Vec::new())
        .await;
    assert_eq!(posted.status, StatusCode::NOT_FOUND);
    let doubled = anonymous
        .exchange("GET", "//example.test/", "text/html", Vec::new())
        .await;
    assert_eq!(doubled.status, StatusCode::NOT_FOUND);

    // Creating a repository with an empty name re-renders the form with the message beside
    // the name field, keeps the draft and keeps the 400 status.
    let (status, html) = bob
        .form(
            "/new",
            &[
                ("name", ""),
                ("description", "kept draft"),
                ("visibility", "public"),
                ("owner_org_id", ""),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{html}");
    assert!(html.contains("id=\"repo-name-error\""), "{html}");
    assert!(html.contains("aria-invalid=\"true\""), "{html}");
    assert!(html.contains("kept draft"), "{html}");
    // The rule is stated once: the message replaces the hint, and the field describes itself
    // only by the message.
    assert!(!html.contains("id=\"repo-name-help\""), "{html}");
    assert!(
        html.contains("aria-describedby=\"repo-name-error\""),
        "{html}"
    );
    // The server message is shown, not a browser bubble, so the input is not marked required.
    assert!(!html.contains(" required="), "{html}");
    // The page-level banner is not shown: the message sits beside the name field only.
    assert!(!html.contains("border-red-500/30"), "{html}");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_error_pages_keep_repository_context_and_plain_copy() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_error_pages_keep_repository_context_and_plain_copy() {
    contract(true).await;
}
