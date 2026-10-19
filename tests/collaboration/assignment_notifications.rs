//! Assignment notifications, on SQLite and PostgreSQL. A user who is newly assigned to an issue or
//! pull request hears about it once, through the assignment form or when an issue is created with
//! assignees. Nobody hears about assigning themselves, a repeated assignment or an unassignment.
//! A user who ignores the repository, or is banned, hears nothing.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use super::notification_privacy::notifications_model;
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::{http::StatusCode, Router};
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db::{self, models::repositories::AccessLevel},
    storage::ObjectStore,
};
use database::Database;
use pulls::Client;
use serde_json::{json, Value};
use std::sync::Arc;

/// alice owns `proj`, which is public. bob and carol are collaborators with read access, so
/// both can be assigned.
struct World {
    database: Database,
    _root: tempfile::TempDir,
    router: Router,
    alice: Client,
    bob: Client,
    carol_id: i64,
    bob_id: i64,
    proj: i64,
}

async fn setup(pg: bool) -> World {
    bundle::install();
    let database = Database::new(pg).await;
    database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = database.actor("alice").await;
    let bob_id = database.actor("bob").await;
    let carol_id = database.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let proj = codeyard::backend::repositories::lifecycle::provision(
        (database.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap()
    .id;
    for user in [bob_id, carol_id] {
        db::repositories::access::grant_repo_access(database.pool(), user, proj, AccessLevel::Read)
            .await
            .unwrap();
    }
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
    World {
        database,
        _root: root,
        router,
        alice,
        bob,
        carol_id,
        bob_id,
        proj,
    }
}

/// The assignment notifications a user has, newest first.
async fn assigned(client: &Client) -> Vec<Value> {
    client
        .expect("GET", "/api/notifications", json!(null), StatusCode::OK)
        .await["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["kind"] == "assigned")
        .cloned()
        .collect()
}

async fn assignment_by_form(pg: bool) {
    let mut w = setup(pg).await;
    w.alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Triage", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    let reply = w
        .alice
        .form_reply("/alice/proj/issues/1/assignees", &[("username", "bob")])
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let mine = assigned(&w.bob).await;
    assert_eq!(mine.len(), 1);
    assert_eq!(
        mine[0]["title"],
        "alice assigned you to issue #1 in alice/proj"
    );
    assert_eq!(mine[0]["link"], "/alice/proj/issues/1");
    assert_eq!(mine[0]["body"], "Triage");
    assert_eq!(mine[0]["actor"]["username"], "alice");

    // A repeated assignment, an unassignment and self-assignment notify no one new.
    w.alice
        .form_reply("/alice/proj/issues/1/assignees", &[("username", "bob")])
        .await;
    w.alice
        .form_reply("/alice/proj/issues/1/assignees/bob/remove", &[])
        .await;
    assert_eq!(assigned(&w.bob).await.len(), 1);
    w.alice
        .form_reply("/alice/proj/issues/1/assignees", &[("username", "alice")])
        .await;
    assert!(assigned(&w.alice).await.is_empty());
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_assignment_by_form() {
    assignment_by_form(true).await;
}
#[tokio::test]
async fn sqlite_assignment_by_form() {
    assignment_by_form(false).await;
}

async fn assignment_on_creation(pg: bool) {
    let mut w = setup(pg).await;
    let reply = w
        .alice
        .form_reply(
            "/alice/proj/issues/new",
            &[
                ("intent", "create"),
                ("title", "Filed with owners"),
                ("body", "Body"),
                ("assignees", "bob"),
                ("assignees", "carol"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let mine = assigned(&w.bob).await;
    assert_eq!(mine.len(), 1);
    assert_eq!(
        mine[0]["title"],
        "alice assigned you to issue #1 in alice/proj"
    );
    assert_eq!(mine[0]["link"], "/alice/proj/issues/1");
    let carol = Client::login(&w.router, w.database.pool(), w.carol_id).await;
    assert_eq!(assigned(&carol).await.len(), 1);
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_assignment_on_creation() {
    assignment_on_creation(true).await;
}
#[tokio::test]
async fn sqlite_assignment_on_creation() {
    assignment_on_creation(false).await;
}

async fn ignoring_and_banned_users_hear_nothing(pg: bool) {
    let mut w = setup(pg).await;
    w.alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Quiet", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    db::repositories::engagement::set_watch_level(
        w.database.pool(),
        w.bob_id,
        w.proj,
        "ignoring",
        "",
    )
    .await
    .unwrap();
    w.alice
        .form_reply("/alice/proj/issues/1/assignees", &[("username", "bob")])
        .await;
    assert!(
        assigned(&w.bob).await.is_empty(),
        "an ignored repository notifies nobody"
    );

    // A banned user is assigned, but hears nothing: checked while banned, then unbanned to read.
    w.database
        .execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[w.carol_id])
        .await;
    w.alice
        .form_reply("/alice/proj/issues/1/assignees", &[("username", "carol")])
        .await;
    w.database
        .execute(
            "UPDATE users SET is_banned=FALSE WHERE id=$1",
            &[w.carol_id],
        )
        .await;
    let carol = Client::login(&w.router, w.database.pool(), w.carol_id).await;
    assert!(
        assigned(&carol).await.is_empty(),
        "a banned user hears nothing"
    );
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_ignoring_and_banned_users_hear_nothing() {
    ignoring_and_banned_users_hear_nothing(true).await;
}
#[tokio::test]
async fn sqlite_ignoring_and_banned_users_hear_nothing() {
    ignoring_and_banned_users_hear_nothing(false).await;
}

async fn pull_request_assignment_links_to_the_pull(pg: bool) {
    let mut w = setup(pg).await;
    let main = w.alice.save("main", "README.md", "unborn").await;
    w.alice.branch("topic", &main).await;
    w.alice.save("topic", "feature.txt", &main).await;
    let pull = w.alice.open_pull("Feature", "topic", "main").await;
    let number = pull["number"].as_i64().unwrap();
    let reply = w
        .alice
        .form_reply(
            &format!("/alice/proj/pulls/{number}/assignees"),
            &[("username", "bob")],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let mine = assigned(&w.bob).await;
    assert_eq!(mine.len(), 1);
    assert_eq!(
        mine[0]["title"],
        format!("alice assigned you to pull request #{number} in alice/proj")
    );
    assert_eq!(mine[0]["link"], format!("/alice/proj/pulls/{number}"));
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_pull_request_assignment_links_to_the_pull() {
    pull_request_assignment_links_to_the_pull(true).await;
}
#[tokio::test]
async fn sqlite_pull_request_assignment_links_to_the_pull() {
    pull_request_assignment_links_to_the_pull(false).await;
}

async fn assigned_reason_filters_the_list(pg: bool) {
    let mut w = setup(pg).await;
    w.alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Filter", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    w.alice
        .form_reply("/alice/proj/issues/1/assignees", &[("username", "bob")])
        .await;
    db::notifications::records::create_notification(
        w.database.pool(),
        db::notifications::NewNotification {
            user_id: w.bob_id,
            actor_id: None,
            repo_id: Some(w.proj),
            kind: "mention",
            title: "Someone mentioned bob",
            body: None,
            link: None,
            metadata: &json!({}),
        },
    )
    .await
    .unwrap();
    let only = w
        .bob
        .expect(
            "GET",
            "/api/notifications?reason=assigned",
            json!(null),
            StatusCode::OK,
        )
        .await;
    let kinds: Vec<&str> = only["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["assigned"]);
    let model = notifications_model(&w.bob, "/notifications?reason=assigned").await;
    let reasons = model["reasons"].as_array().unwrap();
    assert_eq!(reasons.len(), 11);
    assert!(reasons
        .iter()
        .any(|reason| reason["slug"] == "assigned" && reason["label"] == "Assigned"));
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_assigned_reason_filters_the_list() {
    assigned_reason_filters_the_list(true).await;
}
#[tokio::test]
async fn sqlite_assigned_reason_filters_the_list() {
    assigned_reason_filters_the_list(false).await;
}
