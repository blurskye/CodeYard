//! The header's unread badge, on SQLite and PostgreSQL. A signed-in page shows the unread inbox
//! count on the Inbox icon and on the phone menu, and the count follows mark-read and done. A
//! repository the reader cannot read is never counted, the count caps at 99+, and a signed-out
//! page shows no badge.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
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
use serde_json::json;
use std::sync::Arc;

/// alice owns `proj`, which is public, and `vault`, which is private. bob reads the account
/// notifications he is given, and `vault` once a test grants it.
struct World {
    database: Database,
    _root: tempfile::TempDir,
    router: Router,
    bob: Client,
    bob_id: i64,
    vault: i64,
}

async fn setup(pg: bool) -> World {
    bundle::install();
    let database = Database::new(pg).await;
    database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = database.actor("alice").await;
    let bob_id = database.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let mut vault = 0;
    for (name, private) in [("proj", false), ("vault", true)] {
        let repo = codeyard::backend::repositories::lifecycle::provision(
            (database.pool(), None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
            name,
            None,
            private,
            None,
        )
        .await
        .unwrap();
        if name == "vault" {
            vault = repo.id;
        }
    }
    let router = create_router(AppState::new(
        database.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let bob = Client::login(&router, database.pool(), bob_id).await;
    World {
        database,
        _root: root,
        router,
        bob,
        bob_id,
        vault,
    }
}

async fn notify(database: &Database, user: i64, repo: Option<i64>, title: &str) -> i64 {
    db::notifications::records::create_notification(
        database.pool(),
        db::notifications::NewNotification {
            user_id: user,
            actor_id: None,
            repo_id: repo,
            kind: "user_followed",
            title,
            body: None,
            link: None,
            metadata: &json!({}),
        },
    )
    .await
    .unwrap()
    .id
}

/// The header's desktop Inbox icon names the count, so the badge is checked through it.
fn names_unread(html: &str, count: &str) -> bool {
    html.contains(&format!("aria-label=\"Notifications, {count} unread\""))
}

/// No badge and no phone dot: the icon is only named, with no count element.
fn shows_no_badge(html: &str) -> bool {
    html.contains("aria-label=\"Notifications\"") && !html.contains("class=\"site-count-badge\"")
}

async fn badge_follows_marks(pg: bool) {
    let mut w = setup(pg).await;
    let first = notify(&w.database, w.bob_id, None, "first").await;
    let second = notify(&w.database, w.bob_id, None, "second").await;
    let read = notify(&w.database, w.bob_id, None, "read").await;
    let done = notify(&w.database, w.bob_id, None, "done").await;
    w.bob
        .form_reply(&format!("/notifications/{read}/read"), &[])
        .await;
    w.bob
        .expect(
            "PATCH",
            &format!("/api/notifications/{done}/done"),
            json!({"done": true}),
            StatusCode::OK,
        )
        .await;
    // Unread, not done: the first two. Read and done rows are not counted.
    let (status, html) = w.bob.page("/notifications").await;
    assert_eq!(status, StatusCode::OK);
    assert!(names_unread(&html, "2"), "two unread inbox items");
    assert!(
        html.contains("class=\"site-menu-dot\""),
        "the phone menu shows a dot"
    );
    assert!(
        html.contains("class=\"site-count-badge"),
        "the Inbox icon shows the count"
    );

    w.bob
        .expect(
            "PATCH",
            &format!("/api/notifications/{first}/done"),
            json!({"done": true}),
            StatusCode::OK,
        )
        .await;
    let (_, html) = w.bob.page("/notifications").await;
    assert!(names_unread(&html, "1"), "a done item leaves the count");

    // Marking the last one read clears the badge entirely.
    w.bob
        .form_reply(&format!("/notifications/{second}/read"), &[])
        .await;
    let (_, html) = w.bob.page("/notifications").await;
    assert!(shows_no_badge(&html), "no unread inbox items, no badge");
    assert!(!html.contains("class=\"site-menu-dot\""));

    // Undoing a done item returns it to the inbox as unread.
    w.bob
        .expect(
            "PATCH",
            &format!("/api/notifications/{done}/done"),
            json!({"done": false}),
            StatusCode::OK,
        )
        .await;
    let (_, html) = w.bob.page("/notifications").await;
    assert!(
        names_unread(&html, "1"),
        "an undone item is an unread inbox item"
    );
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_badge_follows_marks() {
    badge_follows_marks(true).await;
}
#[tokio::test]
async fn sqlite_badge_follows_marks() {
    badge_follows_marks(false).await;
}

async fn badge_skips_repositories_the_reader_cannot_read(pg: bool) {
    let mut w = setup(pg).await;
    db::repositories::access::grant_repo_access(
        w.database.pool(),
        w.bob_id,
        w.vault,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    notify(&w.database, w.bob_id, Some(w.vault), "vault one").await;
    notify(&w.database, w.bob_id, Some(w.vault), "vault two").await;
    notify(&w.database, w.bob_id, None, "account").await;
    let (_, html) = w.bob.page("/notifications").await;
    assert!(names_unread(&html, "3"));

    // Losing access to the repository hides its history, so the count drops with it.
    db::repositories::access::revoke_repo_access(w.database.pool(), w.bob_id, w.vault)
        .await
        .unwrap();
    let (_, html) = w.bob.page("/notifications").await;
    assert!(names_unread(&html, "1"), "hidden history is not counted");
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_badge_skips_repositories_the_reader_cannot_read() {
    badge_skips_repositories_the_reader_cannot_read(true).await;
}
#[tokio::test]
async fn sqlite_badge_skips_repositories_the_reader_cannot_read() {
    badge_skips_repositories_the_reader_cannot_read(false).await;
}

async fn badge_caps_at_99_plus_and_signed_out_pages_have_none(pg: bool) {
    let mut w = setup(pg).await;
    for index in 0..100 {
        notify(&w.database, w.bob_id, None, &format!("item {index}")).await;
    }
    let (_, html) = w.bob.page("/notifications").await;
    assert!(names_unread(&html, "99+"), "more than 99 shows as 99+");
    assert!(
        !html.contains("Notifications, 100 unread"),
        "the exact count is not shown past 99"
    );

    let anonymous = Client::anonymous(&w.router);
    let (status, html) = anonymous.page("/explore").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!html.contains("class=\"site-count-badge\""));
    assert!(!html.contains("class=\"site-menu-dot\""));
    assert!(!html.contains("href=\"/notifications\""));
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_badge_caps_at_99_plus_and_signed_out_pages_have_none() {
    badge_caps_at_99_plus_and_signed_out_pages_have_none(true).await;
}
#[tokio::test]
async fn sqlite_badge_caps_at_99_plus_and_signed_out_pages_have_none() {
    badge_caps_at_99_plus_and_signed_out_pages_have_none(false).await;
}
