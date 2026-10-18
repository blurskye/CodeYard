//! GitHub-style references in Markdown over HTTP. A reader gets links only to the repositories it
//! can read: a private or missing repository reads as plain text, and its name does not reach the
//! page model. `/issues/N` reaches a pull request. On SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;

use axum::http::StatusCode;
use base64::Engine;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use codeyard::webui::app::{Page, PageEnvelope};
use codeyard::webui::pages::repos::issues::Page as Collaboration;
use codeyard::webui::pages::repos::page::Page as RepositoryPage;
use database::Database;
use pulls::Client;
use serde_json::json;
use std::sync::Arc;

/// The page model of `path`, requested the way the browser asks for it.
async fn page_model(client: &Client, path: &str) -> PageEnvelope {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    let response = client
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("Cookie", format!("session_id={}", client.session))
                .header("Accept", "application/vnd.codeyard.page+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn references(pg: bool) {
    // The native pages render through the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    for (name, private) in [
        ("proj", false),
        ("open", false),
        ("vault", true),
        ("notes", false),
    ] {
        codeyard::backend::repositories::lifecycle::provision(
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

    for (repo, title) in [("proj", "Target"), ("open", "Open"), ("vault", "Secret")] {
        alice
            .expect(
                "POST",
                &format!("/api/repos/alice/{repo}/issues"),
                json!({"title": title, "body": "Body"}),
                StatusCode::CREATED,
            )
            .await;
    }
    let body = "See #1, @bob and @nobody. Private: alice/vault#1. Missing: alice/nothing#1. \
        Readable: alice/open#1. Escaped: \\#1. Code: `#1`. Commit abcdef1.";
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Refs", "body": body}),
            StatusCode::CREATED,
        )
        .await;

    let (status, bob_html) = bob.page("/alice/proj/issues/2").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "<a href=\"/alice/proj/issues/1\" class=\"markdown-reference\">#1</a>",
        "<a href=\"/bob\" class=\"markdown-reference\">@bob</a>",
        "Private: alice/vault#1.",
        "Missing: alice/nothing#1.",
        "<a href=\"/alice/open/issues/1\" class=\"markdown-reference\">alice/open#1</a>",
        "Escaped: #1.",
        "<code class=\"rounded border border-border bg-muted px-1 py-0.5 text-[0.85em] text-foreground\">#1</code>",
        "<a href=\"/alice/proj/commit/abcdef1\" class=\"markdown-reference markdown-commit\" title=\"abcdef1\">abcdef1</a>",
    ] {
        assert!(bob_html.contains(expected), "missing {expected}: {bob_html}");
    }
    assert!(
        !bob_html.contains("href=\"/alice/vault"),
        "private repository linked"
    );
    assert!(
        !bob_html.contains("href=\"/alice/nothing"),
        "missing repository linked"
    );
    assert!(
        !bob_html.contains("href=\"/nobody\""),
        "unknown user linked"
    );

    let (_, alice_html) = alice.page("/alice/proj/issues/2").await;
    assert!(
        alice_html.contains(
            "<a href=\"/alice/vault/issues/1\" class=\"markdown-reference\">alice/vault#1</a>"
        ),
        "{alice_html}"
    );

    let envelope = page_model(&bob, "/alice/proj/issues/2").await;
    let Page::Collaboration(page) = envelope.page else {
        panic!("expected a collaboration page")
    };
    let Collaboration::Detail { references, .. } = *page else {
        panic!("expected a detail page")
    };
    assert_eq!(references.users, vec!["bob".to_owned()]);
    let names: Vec<&str> = references
        .repositories
        .iter()
        .map(|repo| repo.name.as_str())
        .collect();
    assert_eq!(names, vec!["open"]);
    let model = serde_json::to_string(&references).unwrap();
    assert!(
        !model.contains("vault") && !model.contains("nothing"),
        "{model}"
    );

    // A pull request shares the issue number sequence, so `/issues/N` redirects to it.
    let main = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature", &main).await;
    alice.save("feature", "feature.txt", &main).await;
    let pull = alice.open_pull("Feature", "feature", "main").await;
    let number = pull["number"].as_i64().unwrap();
    let redirect = bob
        .exchange(
            "GET",
            &format!("/alice/proj/issues/{number}"),
            "text/html",
            Vec::new(),
        )
        .await;
    assert!(redirect.status.is_redirection(), "{}", redirect.status);
    assert_eq!(
        redirect.location.as_deref(),
        Some(format!("/alice/proj/pulls/{number}").as_str())
    );
    let issue = anonymous
        .exchange("GET", "/alice/proj/issues/1", "text/html", Vec::new())
        .await;
    assert_eq!(issue.status, StatusCode::OK);
    let missing = anonymous
        .exchange("GET", "/alice/proj/issues/99", "text/html", Vec::new())
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    // The file editor previews the way the saved file links. A first load resolves the stored
    // text; a preview resolves the submitted draft, which is what the preview shows.
    let notes_issue = alice
        .expect(
            "POST",
            "/api/repos/alice/notes/issues",
            json!({"title": "Notes", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(notes_issue["number"], 1);
    let stored = "Stored: #1 and alice/open#1.\n";
    alice
        .expect(
            "POST",
            "/api/repos/alice/notes/contents/notes.md",
            json!({
                "content": base64::engine::general_purpose::STANDARD.encode(stored),
                "message": "Add notes",
                "branch": "main",
                "expected_revision": "unborn",
            }),
            StatusCode::CREATED,
        )
        .await;
    let envelope = page_model(&alice, "/alice/notes/edit/main/notes.md").await;
    let Page::Repositories(page) = envelope.page else {
        panic!("expected a repository page")
    };
    let RepositoryPage::Editor { editor, .. } = page else {
        panic!("expected an editor page")
    };
    assert_eq!(editor.references.last_number, 1);
    assert!(
        editor.references.users.is_empty(),
        "{:?}",
        editor.references.users
    );
    let names: Vec<&str> = editor
        .references
        .repositories
        .iter()
        .map(|repo| repo.name.as_str())
        .collect();
    assert_eq!(names, vec!["open"]);

    let notes = db::repositories::records::get_repository(f.pool(), "alice", "notes")
        .await
        .unwrap()
        .unwrap();
    let notes_id = notes.id.to_string();
    let preview = alice
        .form_reply(
            "/alice/notes/_forms/write-file",
            &[
                ("repository_id", notes_id.as_str()),
                ("branch", "main"),
                ("path", "notes.md"),
                ("creating", "no"),
                ("expected_revision", editor.expected_revision.as_str()),
                ("message", "Update notes.md"),
                (
                    "content",
                    "Draft: #1, @bob, alice/open#1 and alice/vault#1.",
                ),
                ("intent", "preview"),
            ],
        )
        .await;
    assert_eq!(preview.status, StatusCode::OK);
    let html = preview.text();
    for expected in [
        "<a href=\"/alice/notes/issues/1\" class=\"markdown-reference\">#1</a>",
        "<a href=\"/bob\" class=\"markdown-reference\">@bob</a>",
        "<a href=\"/alice/open/issues/1\" class=\"markdown-reference\">alice/open#1</a>",
        "<a href=\"/alice/vault/issues/1\" class=\"markdown-reference\">alice/vault#1</a>",
    ] {
        assert!(html.contains(expected), "missing {expected}: {html}");
    }
    // A file that is not Markdown is not previewed as a document, so its editor links nothing,
    // even where its text would link in a Markdown file.
    let text_file = "Stored: #1 and alice/open#1.\n";
    alice
        .expect(
            "POST",
            "/api/repos/alice/notes/contents/notes.txt",
            json!({
                "content": base64::engine::general_purpose::STANDARD.encode(text_file),
                "message": "Add notes text",
                "branch": "main",
                "expected_revision": editor.expected_revision.clone(),
            }),
            StatusCode::CREATED,
        )
        .await;
    let envelope = page_model(&alice, "/alice/notes/edit/main/notes.txt").await;
    let Page::Repositories(page) = envelope.page else {
        panic!("expected a repository page")
    };
    let RepositoryPage::Editor { editor, .. } = page else {
        panic!("expected an editor page")
    };
    assert!(editor.references.repo.is_empty(), "{:?}", editor.references);
    // A new file's model carries this repository's own counter, so its hydrated preview can link
    // #N once the typed path is Markdown.
    let envelope = page_model(&alice, "/alice/notes/new/main").await;
    let Page::Repositories(page) = envelope.page else {
        panic!("expected a repository page")
    };
    let RepositoryPage::Editor { editor, .. } = page else {
        panic!("expected an editor page")
    };
    assert!(editor.creating);
    assert_eq!(editor.references.last_number, 1);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_references_link_only_readable_targets() {
    references(false).await;
}

#[tokio::test]
async fn postgres_references_link_only_readable_targets() {
    references(true).await;
}
