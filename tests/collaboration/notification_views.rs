//! The notification tabs and their marks, through the JSON API and the native forms, on SQLite
//! and PostgreSQL. Saved and done move items between tabs, the unread count leaves done items
//! out, filters narrow a tab without changing its counts, and a user never changes another
//! user's marks or history they can no longer read.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use super::notification_privacy::notifications_model;
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::http::StatusCode;
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

const FORM: &str = "application/x-www-form-urlencoded";

/// alice owns `proj` and `docs`, which are public, and `vault`, which is private. Bob has no
/// repositories of his own.
struct World {
    database: Database,
    _root: tempfile::TempDir,
    alice: Client,
    bob: Client,
    alice_id: i64,
    bob_id: i64,
    proj: i64,
    docs: i64,
    vault: i64,
}

async fn setup(pg: bool) -> World {
    bundle::install();
    let database = Database::new(pg).await;
    database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = database.actor("alice").await;
    let bob_id = database.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let mut repositories = Vec::new();
    for (name, private) in [("proj", false), ("docs", false), ("vault", true)] {
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
        repositories.push(repo.id);
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
        alice,
        bob,
        alice_id,
        bob_id,
        proj: repositories[0],
        docs: repositories[1],
        vault: repositories[2],
    }
}

async fn notify(database: &Database, user: i64, repo: Option<i64>, kind: &str, title: &str) -> i64 {
    db::notifications::records::create_notification(
        database.pool(),
        db::notifications::NewNotification {
            user_id: user,
            actor_id: None,
            repo_id: repo,
            kind,
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

/// The notification IDs of a JSON list, newest first.
async fn listed(client: &Client, path: &str) -> Vec<i64> {
    client
        .expect("GET", path, json!(null), StatusCode::OK)
        .await["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_i64().unwrap())
        .collect()
}

async fn saved_and_done_marks(pg: bool) {
    let mut w = setup(pg).await;
    let mention = notify(
        &w.database,
        w.alice_id,
        Some(w.proj),
        "mention",
        "Look at the build",
    )
    .await;
    let comment = notify(
        &w.database,
        w.alice_id,
        Some(w.proj),
        "issue_comment",
        "Thread reply",
    )
    .await;
    let follow = notify(
        &w.database,
        w.alice_id,
        None,
        "user_followed",
        "Bob followed you",
    )
    .await;
    let inbox = w
        .alice
        .expect("GET", "/api/notifications", json!(null), StatusCode::OK)
        .await;
    assert_eq!(
        inbox["view_counts"],
        json!({"inbox": 3, "saved": 0, "done": 0})
    );
    assert_eq!(inbox["unread_count"], 3);

    let saved = w
        .alice
        .expect(
            "PATCH",
            &format!("/api/notifications/{mention}/save"),
            json!({"saved": true}),
            StatusCode::OK,
        )
        .await;
    assert!(saved["notification"]["saved_at"].is_string());
    assert_eq!(saved["unread_count"], 3);
    let again = w
        .alice
        .expect(
            "PATCH",
            &format!("/api/notifications/{mention}/save"),
            json!({"saved": true}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        again["notification"]["saved_at"], saved["notification"]["saved_at"],
        "saving twice keeps the first time"
    );
    assert_eq!(
        listed(&w.alice, "/api/notifications?view=saved").await,
        vec![mention]
    );

    // A done item leaves the inbox and the unread count, and joins the done tab.
    let done = w
        .alice
        .expect(
            "PATCH",
            &format!("/api/notifications/{comment}/done"),
            json!({"done": true}),
            StatusCode::OK,
        )
        .await;
    assert!(done["notification"]["done_at"].is_string());
    assert_eq!(done["unread_count"], 2);
    assert_eq!(
        listed(&w.alice, "/api/notifications").await,
        vec![follow, mention]
    );
    assert_eq!(
        listed(&w.alice, "/api/notifications?view=done").await,
        vec![comment]
    );
    let counts = w
        .alice
        .expect("GET", "/api/notifications", json!(null), StatusCode::OK)
        .await;
    assert_eq!(
        counts["view_counts"],
        json!({"inbox": 2, "saved": 1, "done": 1})
    );

    // Undo returns it to the inbox; unsave empties the saved tab.
    w.alice
        .expect(
            "PATCH",
            &format!("/api/notifications/{comment}/done"),
            json!({"done": false}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        listed(&w.alice, "/api/notifications").await,
        vec![follow, comment, mention]
    );
    w.alice
        .expect(
            "PATCH",
            &format!("/api/notifications/{mention}/save"),
            json!({"saved": false}),
            StatusCode::OK,
        )
        .await;
    assert!(listed(&w.alice, "/api/notifications?view=saved")
        .await
        .is_empty());

    w.alice
        .expect(
            "PATCH",
            "/api/notifications/999999/done",
            json!({"done": true}),
            StatusCode::NOT_FOUND,
        )
        .await;
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_saved_and_done_marks() {
    saved_and_done_marks(true).await;
}
#[tokio::test]
async fn sqlite_saved_and_done_marks() {
    saved_and_done_marks(false).await;
}

async fn filters_narrow_each_tab(pg: bool) {
    let mut w = setup(pg).await;
    let build = notify(
        &w.database,
        w.alice_id,
        Some(w.proj),
        "mention",
        "Please look at the build",
    )
    .await;
    let handbook = notify(
        &w.database,
        w.alice_id,
        Some(w.docs),
        "issue_comment",
        "Ship the handbook",
    )
    .await;
    let review = notify(
        &w.database,
        w.alice_id,
        Some(w.docs),
        "pull_request_review_requested",
        "Review requested",
    )
    .await;
    let follow = notify(
        &w.database,
        w.alice_id,
        None,
        "user_followed",
        "Bob followed you",
    )
    .await;
    assert_eq!(
        listed(&w.alice, "/api/notifications?reason=comment").await,
        vec![handbook]
    );
    assert_eq!(
        listed(&w.alice, &format!("/api/notifications?repo={}", w.docs)).await,
        vec![review, handbook]
    );
    assert_eq!(
        listed(&w.alice, "/api/notifications?q=BUILD").await,
        vec![build]
    );
    // Search matches the repository name too.
    assert_eq!(
        listed(&w.alice, "/api/notifications?q=docs").await,
        vec![review, handbook]
    );
    assert_eq!(
        listed(
            &w.alice,
            &format!("/api/notifications?reason=review_requested&repo={}", w.docs)
        )
        .await,
        vec![review]
    );
    assert_eq!(
        listed(&w.alice, "/api/notifications?q=%20%20").await,
        vec![follow, review, handbook, build]
    );
    // Filters narrow the list; the tab counts stay whole.
    let filtered = w
        .alice
        .expect(
            "GET",
            "/api/notifications?reason=comment",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(filtered["view_counts"]["inbox"], 4);
    for path in [
        "/api/notifications?view=archive",
        "/api/notifications?reason=nonsense",
        "/api/notifications?repo=abc",
    ] {
        let (status, _) = w
            .alice
            .send("GET", path, "application/json", String::new())
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }

    // The native page shows the same list with its menus filled in.
    let model = notifications_model(&w.alice, "/notifications?reason=comment").await;
    assert_eq!(model["notifications"].as_array().unwrap().len(), 1);
    assert_eq!(model["notifications"][0]["repository"], "alice/docs");
    assert_eq!(model["reason"], "comment");
    assert_eq!(model["reasons"].as_array().unwrap().len(), 11);
    let names: Vec<&str> = model["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alice/docs", "alice/proj"]);
    assert!(model["older"].is_null());
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_filters_narrow_each_tab() {
    filters_narrow_each_tab(true).await;
}
#[tokio::test]
async fn sqlite_filters_narrow_each_tab() {
    filters_narrow_each_tab(false).await;
}

async fn mark_all_done_follows_the_filter(pg: bool) {
    let mut w = setup(pg).await;
    let first = notify(&w.database, w.alice_id, Some(w.proj), "mention", "one").await;
    let second = notify(
        &w.database,
        w.alice_id,
        Some(w.proj),
        "issue_comment",
        "two",
    )
    .await;
    let third = notify(&w.database, w.alice_id, Some(w.docs), "mention", "three").await;
    w.alice
        .expect(
            "PATCH",
            &format!("/api/notifications/{second}/save"),
            json!({"saved": true}),
            StatusCode::OK,
        )
        .await;
    // The native form marks the mentions done and returns to the mention list.
    let reply = w
        .alice
        .exchange(
            "POST",
            "/notifications/done-all",
            FORM,
            format!("csrf_token={}&reason=mention", w.alice.csrf).into_bytes(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.location.as_deref(),
        Some("/notifications?reason=mention")
    );
    assert_eq!(listed(&w.alice, "/api/notifications").await, vec![second]);
    assert_eq!(
        listed(&w.alice, "/api/notifications?view=done").await,
        vec![third, first]
    );
    // The JSON action works on the saved tab, and its count says what it changed.
    let bulk = w
        .alice
        .expect(
            "POST",
            "/api/notifications/done-all?view=saved",
            json!({}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(bulk["updated"], 1);
    assert_eq!(bulk["unread_count"], 0);
    assert!(listed(&w.alice, "/api/notifications").await.is_empty());
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_mark_all_done_follows_the_filter() {
    mark_all_done_follows_the_filter(true).await;
}
#[tokio::test]
async fn sqlite_mark_all_done_follows_the_filter() {
    mark_all_done_follows_the_filter(false).await;
}

/// Another user cannot change these marks, and a repository the recipient can no longer read
/// keeps its history exactly as it was, even through the recipient's own bulk action.
async fn other_users_and_hidden_history_are_untouched(pg: bool) {
    let mut w = setup(pg).await;
    let mine = notify(
        &w.database,
        w.alice_id,
        Some(w.proj),
        "mention",
        "alice's item",
    )
    .await;
    db::repositories::access::grant_repo_access(
        w.database.pool(),
        w.bob_id,
        w.vault,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    let account = notify(&w.database, w.bob_id, None, "user_followed", "bob's item").await;
    let vault_one = notify(&w.database, w.bob_id, Some(w.vault), "mention", "vault one").await;
    let vault_two = notify(&w.database, w.bob_id, Some(w.vault), "mention", "vault two").await;
    db::repositories::access::revoke_repo_access(w.database.pool(), w.bob_id, w.vault)
        .await
        .unwrap();

    // Bob sees his account item only: the vault history is hidden from him.
    assert_eq!(listed(&w.bob, "/api/notifications").await, vec![account]);
    w.bob
        .expect(
            "PATCH",
            &format!("/api/notifications/{mine}/save"),
            json!({"saved": true}),
            StatusCode::NOT_FOUND,
        )
        .await;
    let reply = w
        .bob
        .exchange(
            "POST",
            &format!("/notifications/{mine}/done"),
            FORM,
            format!("csrf_token={}&done=1", w.bob.csrf).into_bytes(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    w.bob
        .expect(
            "PATCH",
            &format!("/api/notifications/{vault_one}/done"),
            json!({"done": true}),
            StatusCode::NOT_FOUND,
        )
        .await;

    // Bob's bulk action marks only what he can see.
    let bulk = w
        .bob
        .expect(
            "POST",
            "/api/notifications/done-all",
            json!({}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(bulk["updated"], 1);
    assert_eq!(bulk["unread_count"], 0);
    // Granting access again shows the rows as they were: neither done nor saved.
    db::repositories::access::grant_repo_access(
        w.database.pool(),
        w.bob_id,
        w.vault,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    let everything = w
        .bob
        .expect("GET", "/api/notifications", json!(null), StatusCode::OK)
        .await;
    for hidden in [vault_one, vault_two] {
        let row = everything["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == hidden)
            .unwrap_or_else(|| panic!("{hidden} is visible again"));
        assert!(row["done_at"].is_null(), "{hidden} must not be done");
        assert!(row["saved_at"].is_null(), "{hidden} must not be saved");
    }
    // Alice's item is still hers, unsaved and in her inbox.
    assert_eq!(listed(&w.alice, "/api/notifications").await, vec![mine]);
    assert_eq!(
        listed(&w.alice, "/api/notifications?view=saved").await,
        Vec::<i64>::new()
    );
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_other_users_and_hidden_history_are_untouched() {
    other_users_and_hidden_history_are_untouched(true).await;
}
#[tokio::test]
async fn sqlite_other_users_and_hidden_history_are_untouched() {
    other_users_and_hidden_history_are_untouched(false).await;
}

/// The native forms act on one row and return to the list they came from, with the
/// filters and the tab kept. A form for an unknown row is not found, and a bad tab is refused.
async fn native_forms_return_to_their_list(pg: bool) {
    let mut w = setup(pg).await;
    let item = notify(&w.database, w.alice_id, Some(w.proj), "mention", "native").await;
    let csrf = w.alice.csrf.clone();
    let save = w
        .alice
        .exchange(
            "POST",
            &format!("/notifications/{item}/save"),
            FORM,
            format!("csrf_token={csrf}&saved=1&view=inbox&reason=mention&unread_only=1")
                .into_bytes(),
        )
        .await;
    assert_eq!(save.status, StatusCode::SEE_OTHER);
    assert_eq!(
        save.location.as_deref(),
        Some("/notifications?reason=mention&unread_only=1")
    );
    let saved = notifications_model(&w.alice, "/notifications?view=saved").await;
    assert_eq!(saved["notifications"][0]["id"], item);
    assert_eq!(saved["notifications"][0]["saved"], true);
    assert_eq!(saved["counts"]["saved"], 1);

    let done = w
        .alice
        .exchange(
            "POST",
            &format!("/notifications/{item}/done"),
            FORM,
            format!("csrf_token={csrf}&done=1&view=saved").into_bytes(),
        )
        .await;
    assert_eq!(done.location.as_deref(), Some("/notifications?view=saved"));
    let done_model = notifications_model(&w.alice, "/notifications?view=done").await;
    assert_eq!(done_model["notifications"][0]["id"], item);
    assert_eq!(done_model["counts"]["inbox"], 0);

    let undo = w
        .alice
        .exchange(
            "POST",
            &format!("/notifications/{item}/done"),
            FORM,
            format!("csrf_token={csrf}&done=0&view=done").into_bytes(),
        )
        .await;
    assert_eq!(undo.location.as_deref(), Some("/notifications?view=done"));
    assert_eq!(listed(&w.alice, "/api/notifications").await, vec![item]);

    let (_, page) = w.alice.page("/notifications").await;
    assert!(page.contains(&format!("action=\"/notifications/{item}/save\"")));
    assert!(page.contains(&format!("action=\"/notifications/{item}/done\"")));
    assert!(page.contains("action=\"/notifications/done-all\""));

    let missing = w
        .alice
        .exchange(
            "POST",
            "/notifications/999999/save",
            FORM,
            format!("csrf_token={csrf}&saved=1").into_bytes(),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let bad_tab = w
        .alice
        .exchange(
            "POST",
            &format!("/notifications/{item}/done"),
            FORM,
            format!("csrf_token={csrf}&done=1&view=archive").into_bytes(),
        )
        .await;
    assert_eq!(bad_tab.status, StatusCode::BAD_REQUEST);
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_native_forms_return_to_their_list() {
    native_forms_return_to_their_list(true).await;
}
#[tokio::test]
async fn sqlite_native_forms_return_to_their_list() {
    native_forms_return_to_their_list(false).await;
}

/// Search ignores case beyond ASCII: "échec" finds "Échec". PostgreSQL folds case by the database
/// locale, as its repository search does, so this check is for SQLite.
#[tokio::test]
async fn sqlite_search_ignores_case_beyond_ascii() {
    let w = setup(false).await;
    let failed = notify(
        &w.database,
        w.alice_id,
        Some(w.proj),
        "ci_completed",
        "Échec du build",
    )
    .await;
    notify(
        &w.database,
        w.alice_id,
        Some(w.proj),
        "ci_completed",
        "Build passed",
    )
    .await;
    for query in ["%C3%A9chec", "%C3%89CHEC", "%C3%89chec"] {
        assert_eq!(
            listed(&w.alice, &format!("/api/notifications?q={query}")).await,
            vec![failed],
            "query {query}"
        );
    }
}

/// A done notification has left the inbox, so it is never unread: the Unread filter and the header
/// count leave it out. The search text finds a notification by its subject as well as its sentence.
async fn done_is_never_unread_and_search_finds_the_subject(pg: bool) {
    let mut w = setup(pg).await;
    let subject = db::notifications::records::create_notification(
        w.database.pool(),
        db::notifications::NewNotification {
            user_id: w.alice_id,
            actor_id: None,
            repo_id: Some(w.proj),
            kind: "issue_comment",
            title: "bob commented on issue #1",
            body: None,
            link: None,
            metadata: &json!({"subject": "Crash when saving an empty file"}),
        },
    )
    .await
    .unwrap()
    .id;
    let other = notify(
        &w.database,
        w.alice_id,
        Some(w.proj),
        "mention",
        "carol mentioned you",
    )
    .await;
    assert_eq!(
        listed(&w.alice, "/api/notifications?q=crash").await,
        vec![subject]
    );
    let inbox = w
        .alice
        .expect("GET", "/api/notifications", json!(null), StatusCode::OK)
        .await;
    assert_eq!(inbox["unread_count"], 2);

    w.alice
        .expect(
            "PATCH",
            &format!("/api/notifications/{subject}/done"),
            json!({"done": true}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        listed(&w.alice, "/api/notifications?view=done").await,
        vec![subject]
    );
    assert!(
        listed(&w.alice, "/api/notifications?view=done&unread_only=1")
            .await
            .is_empty()
    );
    assert_eq!(
        listed(&w.alice, "/api/notifications?unread_only=1").await,
        vec![other]
    );
    let inbox = w
        .alice
        .expect("GET", "/api/notifications", json!(null), StatusCode::OK)
        .await;
    assert_eq!(inbox["unread_count"], 1);
    w.database.finish().await;
}

/// A comment on an issue notifies its author with the issue's title as the subject, and the
/// notifications page leads the row with that title.
async fn issue_comment_notice_is_headed_by_the_issue_title(pg: bool) {
    let mut w = setup(pg).await;
    let issue = w
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Crash when saving an empty file"}),
            StatusCode::CREATED,
        )
        .await;
    let number = issue["number"].as_i64().unwrap();
    w.bob
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/issues/{number}/comments"),
            json!({"body": "I reproduced the crash"}),
            StatusCode::CREATED,
        )
        .await;
    let inbox = w
        .alice
        .expect("GET", "/api/notifications", json!(null), StatusCode::OK)
        .await;
    let note = &inbox["notifications"][0];
    assert_eq!(note["kind"], "issue_comment");
    assert_eq!(
        note["metadata"]["subject"],
        "Crash when saving an empty file"
    );
    let (status, html) = w.alice.page("/notifications").await;
    assert_eq!(status, StatusCode::OK);
    // The title link is headed by the subject, and the sentence stays in the row's metadata.
    let title = html
        .split("class=\"notification-title\"")
        .nth(1)
        .and_then(|rest| rest.split("</h2>").next())
        .unwrap_or_default();
    assert!(title.contains("class=\"notification-link\""), "{html}");
    assert!(title.contains("Crash when saving an empty file"), "{html}");
    assert!(!title.contains("commented on issue"), "{html}");
    assert!(html.contains("bob commented on issue #1"), "{html}");
    assert!(html.contains("Comments"), "{html}");
    w.database.finish().await;
}

#[tokio::test]
async fn postgres_done_is_never_unread_and_search_finds_the_subject() {
    done_is_never_unread_and_search_finds_the_subject(true).await;
}
#[tokio::test]
async fn sqlite_done_is_never_unread_and_search_finds_the_subject() {
    done_is_never_unread_and_search_finds_the_subject(false).await;
}
#[tokio::test]
async fn postgres_issue_comment_notice_is_headed_by_the_issue_title() {
    issue_comment_notice_is_headed_by_the_issue_title(true).await;
}
#[tokio::test]
async fn sqlite_issue_comment_notice_is_headed_by_the_issue_title() {
    issue_comment_notice_is_headed_by_the_issue_title(false).await;
}
