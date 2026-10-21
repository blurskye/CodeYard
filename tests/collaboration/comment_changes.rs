//! Editing and deleting comments on issues and pull requests, through the native forms. The author
//! edits. The author or a maintainer deletes. A locked thread and an archived repository refuse
//! both. Each refusal names the reason, and a change that is refused changes nothing. On SQLite and
//! PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client as pulls;
use axum::http::StatusCode;
use codeyard::backend::{db, db::models::AccessLevel};
use database::Database;
use pulls::Client;
use serde_json::json;
use std::time::Duration;

/// `alice/proj` is public and alice owns it. bob has write access, carol maintains the repository
/// and dave has no grant, so dave is an outsider who can only read.
struct World {
    database: Database,
    _root: tempfile::TempDir,
    alice: Client,
    bob: Client,
    carol: Client,
    dave: Client,
}

async fn setup(pg: bool) -> World {
    bundle::install();
    let database = Database::new(pg).await;
    database.complete_setup().await;
    let alice_id = database.actor("alice").await;
    let bob_id = database.actor("bob").await;
    let carol_id = database.actor("carol").await;
    let dave_id = database.actor("dave").await;
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
    db::repositories::access::grant_repo_access(database.pool(), bob_id, proj, AccessLevel::Write)
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(
        database.pool(),
        carol_id,
        proj,
        AccessLevel::Maintain,
    )
    .await
    .unwrap();
    let router = database.router(root.path()).await;
    let alice = Client::login(&router, database.pool(), alice_id).await;
    let bob = Client::login(&router, database.pool(), bob_id).await;
    let carol = Client::login(&router, database.pool(), carol_id).await;
    let dave = Client::login(&router, database.pool(), dave_id).await;
    World {
        database,
        _root: root,
        alice,
        bob,
        carol,
        dave,
    }
}

async fn comment(client: &Client, path: &str, body: &str) -> i64 {
    client
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/{path}/comments"),
            json!({ "body": body }),
            StatusCode::CREATED,
        )
        .await["id"]
        .as_i64()
        .unwrap()
}

/// Posts a form and checks that it was refused with `message`.
async fn refused(client: &Client, path: &str, pairs: &[(&str, &str)], message: &str) {
    let reply = client.form_reply(path, pairs).await;
    assert_eq!(
        reply.status,
        StatusCode::FORBIDDEN,
        "{path}: {}",
        reply.text()
    );
    assert!(reply.text().contains(message), "{path}: {}", reply.text());
}

async fn edit_and_delete_rules(pg: bool) {
    let mut world = setup(pg).await;
    let (alice, bob, carol, dave) = (&world.alice, &world.bob, &world.carol, &world.dave);
    let database = &world.database;
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Bug", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    let issue = "/alice/proj/issues/1";
    let own = comment(alice, "issues/1", "Alice's words").await;
    let theirs = comment(bob, "issues/1", "Bob's words").await;
    let at = |id: i64| format!("{issue}/comments/{id}");

    // Only the author edits: the owner and the other users are refused.
    let rewrite = [("body", "Rewritten")];
    refused(
        bob,
        &format!("{}/edit", at(own)),
        &rewrite,
        "Only the author can edit this comment.",
    )
    .await;
    refused(
        alice,
        &format!("{}/edit", at(theirs)),
        &rewrite,
        "Only the author can edit this comment.",
    )
    .await;
    refused(
        dave,
        &format!("{}/edit", at(theirs)),
        &rewrite,
        "Only the author can edit this comment.",
    )
    .await;

    // The author's edit lands, and the page marks the comment as edited.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let saved = bob
        .form_reply(
            &format!("{}/edit", at(theirs)),
            &[
                ("body", "Bob revised"),
                ("expected_body", "\"Bob's words\""),
            ],
        )
        .await;
    assert!(saved.status.is_redirection(), "{}", saved.text());
    let (_, page) = bob.page(issue).await;
    assert!(page.contains("Bob revised"), "{page}");
    assert!(page.contains("collaboration-edited\""), "{page}");

    // Only the author and a maintainer delete. A reader's delete is refused and the comment stays.
    refused(
        bob,
        &format!("{}/delete", at(own)),
        &[],
        "You cannot delete this comment.",
    )
    .await;
    refused(
        dave,
        &format!("{}/delete", at(theirs)),
        &[],
        "You cannot delete this comment.",
    )
    .await;
    let (_, page) = bob.page(issue).await;
    assert!(page.contains(&format!("comment-{own}")), "{page}");

    // A maintainer deletes someone else's comment; the author deletes their own.
    let removed = carol
        .form_reply(&format!("{}/delete", at(theirs)), &[])
        .await;
    assert!(removed.status.is_redirection(), "{}", removed.text());
    let own_removed = alice.form_reply(&format!("{}/delete", at(own)), &[]).await;
    assert!(
        own_removed.status.is_redirection(),
        "{}",
        own_removed.text()
    );
    let page = bob.page_ok(issue).await;
    assert!(!page.contains(&format!("comment-{theirs}")), "{page}");
    assert!(!page.contains(&format!("comment-{own}")), "{page}");

    // A locked thread refuses both changes, for everyone.
    let locked = comment(bob, "issues/1", "Before the lock").await;
    database
        .execute("UPDATE issues SET locked=TRUE WHERE number=1", &[])
        .await;
    refused(
        bob,
        &format!("{}/edit", at(locked)),
        &rewrite,
        "This conversation is locked.",
    )
    .await;
    refused(
        carol,
        &format!("{}/delete", at(locked)),
        &[],
        "This conversation is locked.",
    )
    .await;
    refused(
        bob,
        &format!("{issue}/comments"),
        &[("body", "After lock")],
        "This conversation is locked.",
    )
    .await;
    let (status, body) = bob
        .api(
            "POST",
            "/api/repos/alice/proj/issues/1/comments",
            json!({"body": "After lock"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "conversation_locked");
    let stored = db::issues::get_issue_comment(database.pool(), locked)
        .await
        .unwrap()
        .unwrap();
    assert!(db::issues::add_issue_comment(
        database.pool(),
        stored.issue_id,
        stored.author_id,
        "After lock"
    )
    .await
    .is_err());
    database
        .execute("UPDATE issues SET locked=FALSE WHERE number=1", &[])
        .await;

    // An archived repository refuses both changes too.
    database
        .execute(
            "UPDATE repositories SET is_archived=TRUE WHERE name='proj'",
            &[],
        )
        .await;
    refused(
        bob,
        &format!("{}/edit", at(locked)),
        &rewrite,
        "This repository is archived and read-only.",
    )
    .await;
    refused(
        carol,
        &format!("{}/delete", at(locked)),
        &[],
        "This repository is archived and read-only.",
    )
    .await;
    let (_, page) = bob.page(issue).await;
    assert!(page.contains(&format!("comment-{locked}")), "{page}");
    world.database.finish().await;
}

async fn pull_request_comment_rules(pg: bool) {
    let mut world = setup(pg).await;
    let base = world.alice.save("main", "README.md", "unborn").await;
    world.alice.branch("topic", &base).await;
    world.alice.save("topic", "topic.txt", &base).await;
    let number = world.alice.open_pull("topic", "topic", "main").await["number"]
        .as_i64()
        .unwrap();
    let page = format!("/alice/proj/pulls/{number}");
    let ours = world
        .alice
        .form_reply(
            &format!("{page}/comments"),
            &[("body", "Alice's question"), ("intent", "comment")],
        )
        .await;
    assert!(ours.status.is_redirection(), "{}", ours.text());
    let alices = ours.id_after("#comment-");
    let theirs = world
        .bob
        .form_reply(
            &format!("{page}/comments"),
            &[("body", "Bob's answer"), ("intent", "comment")],
        )
        .await;
    assert!(theirs.status.is_redirection(), "{}", theirs.text());
    let bobs = theirs.id_after("#comment-");

    refused(
        &world.bob,
        &format!("{page}/comments/{alices}/edit"),
        &[("body", "x"), ("expected_body", "\"Alice's question\"")],
        "Only the author can edit this comment.",
    )
    .await;
    let saved = world
        .bob
        .form_reply(
            &format!("{page}/comments/{bobs}/edit"),
            &[
                ("body", "Bob's revised answer"),
                ("expected_body", "\"Bob's answer\""),
            ],
        )
        .await;
    assert!(saved.status.is_redirection(), "{}", saved.text());
    refused(
        &world.dave,
        &format!("{page}/comments/{bobs}/delete"),
        &[],
        "You cannot delete this comment.",
    )
    .await;
    let removed = world
        .carol
        .form_reply(&format!("{page}/comments/{bobs}/delete"), &[])
        .await;
    assert!(removed.status.is_redirection(), "{}", removed.text());
    let (_, shown) = world.alice.page(&page).await;
    assert!(shown.contains(&format!("comment-{alices}")), "{shown}");
    assert!(!shown.contains(&format!("comment-{bobs}")), "{shown}");

    world
        .database
        .execute(
            "UPDATE repositories SET is_archived=TRUE WHERE name='proj'",
            &[],
        )
        .await;
    refused(
        &world.alice,
        &format!("{page}/comments/{alices}/delete"),
        &[],
        "This repository is archived and read-only.",
    )
    .await;
    world.database.finish().await;
}

async fn late_notifications_and_long_discussions(pg: bool) {
    let mut world = setup(pg).await;
    let issue = world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Long discussion"}),
            StatusCode::CREATED,
        )
        .await;
    let issue_id = issue["id"].as_i64().unwrap();
    let pool = world.database.pool();
    let repo = db::repositories::records::get_repository(pool, "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let alice_id = repo.owner_user_id.unwrap();
    let comment_id = comment(&world.bob, "issues/1", "deleted private text").await;
    let metadata = json!({"comment_id": comment_id});
    let input = |kind| db::notifications::records::NewNotification {
        user_id: alice_id,
        actor_id: None,
        repo_id: Some(repo.id),
        kind,
        title: "A comment",
        body: Some("deleted private text"),
        link: None,
        metadata: &metadata,
    };
    db::notifications::records::create_notification(pool, input("mention"))
        .await
        .unwrap();
    assert!(
        db::issues::delete_issue_comment(pool, alice_id, repo.id, issue_id, comment_id)
            .await
            .unwrap()
    );
    for kind in ["issue_comment", "mention"] {
        assert!(
            db::notifications::records::create_notification(pool, input(kind))
                .await
                .is_err(),
            "late {kind} resurrected a deleted comment"
        );
    }
    world
        .database
        .execute(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<30000)
         INSERT INTO issue_comments(issue_id,author_id,body) SELECT $1,$2,'Earlier comment' FROM n",
            &[issue_id, alice_id],
        )
        .await;
    let last = comment(&world.bob, "issues/1", "Comment beyond thirty thousand").await;
    let (status, page) = world.alice.page("/alice/proj/issues/1?page=1000").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("page=1001"));
    let (status, page) = world.alice.page("/alice/proj/issues/1?page=1001").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Comment beyond thirty thousand"));
    assert!(page.contains(&format!("comment-{last}")));
    world.database.finish().await;
}

#[tokio::test]
async fn late_notifications_and_long_discussions_on_both_engines() {
    late_notifications_and_long_discussions(false).await;
    late_notifications_and_long_discussions(true).await;
}

#[tokio::test]
async fn postgres_edit_and_delete_rules() {
    edit_and_delete_rules(true).await;
}
#[tokio::test]
async fn sqlite_edit_and_delete_rules() {
    edit_and_delete_rules(false).await;
}
#[tokio::test]
async fn postgres_pull_request_comment_rules() {
    pull_request_comment_rules(true).await;
}
#[tokio::test]
async fn sqlite_pull_request_comment_rules() {
    pull_request_comment_rules(false).await;
}
