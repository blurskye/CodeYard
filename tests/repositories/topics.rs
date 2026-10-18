//! Repository topics and the About website through the JSON and native forms, and the Explore
//! topic filter. A save replaces the whole topic set, is validated before it is written, needs
//! write access, and is refused for an archived repository. On SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use database::Database;
use pulls::Client;
use serde_json::{json, Value};
use std::sync::Arc;

/// The page without Dioxus node-id comments, so text can be matched next to its tag.
fn plain(page: &str) -> String {
    let mut out = String::new();
    let mut rest = page;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        rest = rest[start..].split_once("-->").map_or("", |(_, tail)| tail);
    }
    out.push_str(rest);
    out
}

async fn save_about(
    client: &Client,
    description: &str,
    website: &str,
    topics: Value,
) -> (StatusCode, Value) {
    client
        .api(
            "POST",
            "/api/repos/alice/proj/about",
            json!({"description": description, "website": website, "topics": topics}),
        )
        .await
}

async fn topics_save_validate_and_filter(pg: bool) {
    bundle::install();
    let mut database = Database::new(pg).await;
    database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = database.actor("alice").await;
    let bob_id = database.actor("bob").await;
    let carol_id = database.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    codeyard::backend::repositories::lifecycle::provision(
        (database.pool(), None),
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
        database.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, database.pool(), alice_id).await;
    let bob = Client::login(&router, database.pool(), bob_id).await;
    // carol is signed in and can read the public repository, but she cannot write to it.
    let carol = Client::login(&router, database.pool(), carol_id).await;
    let (_, repo) = alice.api("GET", "/api/repos/alice/proj", Value::Null).await;
    let id = repo["id"].as_i64().unwrap();
    let id_text = id.to_string();

    // Names are lowercased and repeated names kept once. The About text comes back as saved.
    let (status, body) = save_about(
        &alice,
        "A self-hosted forge",
        "https://example.com/proj",
        json!(["Rust", "git", "rust"]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["topics"], json!(["git", "rust"]));
    assert_eq!(body["website"], "https://example.com/proj");
    assert_eq!(body["description"], "A self-hosted forge");

    // Each of these is refused, and the earlier About text stays.
    let too_many: Vec<String> = (0..21).map(|n| format!("t{n}")).collect();
    for (website, topics) in [
        ("", json!(["bad topic"])),
        ("", json!(["under_score"])),
        ("", json!(["a".repeat(36)])),
        ("", json!(too_many)),
        ("javascript:alert(1)", json!([])),
        ("ftp://example.com", json!([])),
        ("https://exa mple.com", json!([])),
    ] {
        let (status, body) = save_about(&alice, "changed", website, topics.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{topics} {website} {body}");
        assert_eq!(body["error"], "validation_error");
    }
    let (_, repo) = alice.api("GET", "/api/repos/alice/proj", Value::Null).await;
    assert_eq!(repo["topics"], json!(["git", "rust"]));
    assert_eq!(repo["description"], "A self-hosted forge");

    // A signed-in user without write access gets no answer about the repository's About text.
    let (status, _) = save_about(&carol, "spam", "", json!(["spam"])).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let reply = carol
        .form_reply(
            "/alice/proj/_forms/about",
            &[
                ("repository_id", id_text.as_str()),
                ("description", "spam"),
                ("website", ""),
                ("topics", "spam"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());

    // A collaborator with write access saves the About text, through JSON and the native form.
    db::repositories::access::grant_repo_access(
        database.pool(),
        bob_id,
        id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let (status, body) = save_about(&bob, "Edited by bob", "", json!(["forge"])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["topics"], json!(["forge"]));
    assert!(body["website"].is_null());
    let reply = bob
        .form_reply(
            "/alice/proj/_forms/about",
            &[
                ("repository_id", id_text.as_str()),
                ("description", "From the form"),
                ("website", "https://example.org/docs"),
                ("topics", "Forge, Native  forms"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let (_, repo) = bob.api("GET", "/api/repos/alice/proj", Value::Null).await;
    assert_eq!(repo["topics"], json!(["forge", "forms", "native"]));
    assert_eq!(repo["website"], "https://example.org/docs");
    let reply = bob
        .form_reply(
            "/alice/proj/_forms/about",
            &[
                ("repository_id", id_text.as_str()),
                ("description", "x"),
                ("website", ""),
                // Commas and spaces separate topics, so only the underscore name is refused.
                ("topics", "forge under_score"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    assert!(reply.text().contains("Topics use lowercase letters"));
    let (_, repo) = bob.api("GET", "/api/repos/alice/proj", Value::Null).await;
    assert_eq!(repo["topics"], json!(["forge", "forms", "native"]));

    // Six topics on one repository: Explore shows the first five as chips, and the filter
    // keeps only the repositories that carry the named topic.
    let (status, body) = save_about(
        &bob,
        "Edited by bob",
        "",
        json!(["a1", "a2", "a3", "a4", "a5", "a6"]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, page) = carol.page("/explore").await;
    let page = plain(&page);
    assert!(page.contains("href=\"/explore?topic=a5\""));
    assert!(!page.contains("topic=a6"));
    let (status, page) = carol.page("/explore?topic=A1").await;
    assert_eq!(status, StatusCode::OK);
    let page = plain(&page);
    assert!(page.contains("/alice/proj"));
    assert!(page.contains("aria-label=\"Topic filter\""));
    let (_, page) = carol.page("/explore?topic=rust").await;
    assert!(!plain(&page).contains("/alice/proj"));
    let (_, list) = carol.api("GET", "/api/explore", Value::Null).await;
    assert_eq!(list[0]["topics"].as_array().unwrap().len(), 6);

    // An archived repository refuses the save, even from a writer, and keeps its text.
    database
        .execute(
            &format!("UPDATE repositories SET is_archived=TRUE WHERE id={id}"),
            &[],
        )
        .await;
    let (status, body) = save_about(&bob, "late", "", json!(["late"])).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (_, repo) = alice.api("GET", "/api/repos/alice/proj", Value::Null).await;
    assert_eq!(repo["description"], "Edited by bob");
    assert_eq!(repo["topics"], json!(["a1", "a2", "a3", "a4", "a5", "a6"]));
    database
        .execute(
            &format!("UPDATE repositories SET is_archived=FALSE WHERE id={id}"),
            &[],
        )
        .await;

    // Deleting the repository removes its topics with it.
    database
        .execute(&format!("DELETE FROM repositories WHERE id={id}"), &[])
        .await;
    let topics = db::repositories::topics::repository_topics(database.pool(), &[id])
        .await
        .unwrap();
    assert!(topics.is_empty());
    database.finish().await;
}

#[tokio::test]
async fn postgres_topics_save_validate_and_filter() {
    topics_save_validate_and_filter(true).await;
}
#[tokio::test]
async fn sqlite_topics_save_validate_and_filter() {
    topics_save_validate_and_filter(false).await;
}
