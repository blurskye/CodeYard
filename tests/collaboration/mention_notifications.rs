//! Mention notifications over HTTP and the native forms. A user that new issue, pull request,
//! comment or review text mentions is notified, and so is a user that an edit newly mentions.
//! Nobody who cannot read the repository is notified. On SQLite and PostgreSQL.
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

/// `alice/proj` is public and `alice/vault` is private, and alice owns both. Bob, Carol and Dave
/// can read `proj` and nothing else.
struct World {
    database: Database,
    _root: tempfile::TempDir,
    alice: Client,
    bob: Client,
    carol: Client,
    alice_id: i64,
    bob_id: i64,
    carol_id: i64,
    dave_id: i64,
    /// The commit `main` holds.
    base: String,
}

impl World {
    async fn new(pg: bool) -> Self {
        bundle::install();
        let database = Database::new(pg).await;
        database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
        let alice_id = database.actor("alice").await;
        let bob_id = database.actor("bob").await;
        let carol_id = database.actor("carol").await;
        let dave_id = database.actor("dave").await;
        let root = tempfile::tempdir().unwrap();
        let mut proj = None;
        for (name, private) in [("proj", false), ("vault", true)] {
            let repository = codeyard::backend::repositories::lifecycle::provision(
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
            if name == "proj" {
                proj = Some(repository.id);
            }
        }
        let proj = proj.unwrap();
        for user in [bob_id, carol_id, dave_id] {
            db::repositories::access::grant_repo_access(
                database.pool(),
                user,
                proj,
                db::models::AccessLevel::Read,
            )
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
        let carol = Client::login(&router, database.pool(), carol_id).await;
        let base = alice.save("main", "README.md", "unborn").await;
        Self {
            database,
            _root: root,
            alice,
            bob,
            carol,
            alice_id,
            bob_id,
            carol_id,
            dave_id,
            base,
        }
    }

    fn pool(&self) -> &db::DbPool {
        self.database.pool()
    }

    async fn close(mut self) {
        self.database.finish().await;
    }
}

/// The mention notifications of `user`, as (title, link) pairs.
async fn mentions(world: &World, user: i64) -> Vec<(String, Option<String>)> {
    notices(world, user, "mention")
        .await
        .into_iter()
        .map(|notification| (notification.title, notification.link))
        .collect()
}

/// The notifications of `user` with `kind`.
async fn notices(world: &World, user: i64, kind: &str) -> Vec<db::models::Notification> {
    db::notifications::records::list_notifications_for_user(world.pool(), user, 100, false)
        .await
        .unwrap()
        .into_iter()
        .filter(|notification| notification.kind == kind)
        .collect()
}

/// The sorted field names of a JSON object.
fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

async fn issue_mentions(pg: bool) {
    let world = World::new(pg).await;
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Refs", "body": "Hi @bob, and @alice too"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(
        mentions(&world, world.bob_id).await,
        [(
            "alice mentioned you in alice/proj#1".to_owned(),
            Some("/alice/proj/issues/1".to_owned())
        )]
    );
    // A self-mention notifies nobody.
    assert!(mentions(&world, world.alice_id).await.is_empty());
    world.close().await;
}

/// Deleting a comment takes the mention notices it sent, so their copy of the text goes too.
async fn deleted_comments_take_their_mention_notices(pg: bool) {
    let world = World::new(pg).await;
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Thread", "body": "Opening"}),
            StatusCode::CREATED,
        )
        .await;
    let comment = world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues/1/comments",
            json!({"body": "Over to @bob"}),
            StatusCode::CREATED,
        )
        .await["id"]
        .as_i64()
        .unwrap();
    assert_eq!(mentions(&world, world.bob_id).await.len(), 1);
    let removed = world
        .alice
        .form_reply(
            &format!("/alice/proj/issues/1/comments/{comment}/delete"),
            &[],
        )
        .await;
    assert!(removed.status.is_redirection(), "{}", removed.text());
    assert!(mentions(&world, world.bob_id).await.is_empty());

    let deleted_issue_comment = comment;
    world.alice.branch("topic", &world.base).await;
    world.alice.save("topic", "topic.txt", &world.base).await;
    let pull = world.alice.open_pull("Thread", "topic", "main").await;
    let number = pull["number"].as_i64().unwrap();
    let comment = world
        .alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/comments"),
            json!({"body":"Private text for @bob"}),
            StatusCode::CREATED,
        )
        .await;
    let id = comment["id"].as_i64().unwrap();
    let notices = notices(&world, world.bob_id, "mention").await;
    assert_eq!(notices.len(), 1);
    let metadata = &notices[0].metadata;
    assert_eq!(metadata["pull_request_comment_id"], id);
    assert!(metadata["comment_id"].is_null());
    let removed = world
        .alice
        .form_reply(
            &format!("/alice/proj/pulls/{number}/comments/{id}/delete"),
            &[],
        )
        .await;
    assert!(removed.status.is_redirection(), "{}", removed.text());
    assert!(mentions(&world, world.bob_id).await.is_empty());
    // A delayed recipient from the original fan-out cannot recreate the deleted text.
    let repo = db::repositories::records::get_repository(world.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let delayed = db::notifications::records::create_notification(
        world.pool(),
        db::notifications::records::NewNotification {
            user_id: world.bob_id,
            actor_id: Some(world.alice_id),
            repo_id: Some(repo.id),
            kind: "mention",
            title: "delayed",
            body: Some("Over to @bob"),
            link: None,
            metadata: &json!({"comment_id": deleted_issue_comment}),
        },
    )
    .await;
    assert!(delayed
        .unwrap_err()
        .is::<db::notifications::records::NotificationSuppressed>());
    let delayed_pull = db::notifications::records::create_notification(
        world.pool(),
        db::notifications::records::NewNotification {
            user_id: world.bob_id,
            actor_id: Some(world.alice_id),
            repo_id: Some(repo.id),
            kind: "mention",
            title: "delayed pull",
            body: Some("Private text for @bob"),
            link: None,
            metadata: &json!({"pull_request_comment_id": id}),
        },
    )
    .await;
    assert!(delayed_pull
        .unwrap_err()
        .is::<db::notifications::records::NotificationSuppressed>());
    assert!(mentions(&world, world.bob_id).await.is_empty());
    world.close().await;
}

async fn ignored_mentions(pg: bool) {
    let world = World::new(pg).await;
    let body = "Real @bob.\n\n`@carol` and `@carol`\n\n```\n@carol\n```\n\n\
        [@carol](https://example.com/carol) ![@carol](carol.png) carol@example.com \
        \\@carol <code>@carol</code>";
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Quoted", "body": body}),
            StatusCode::CREATED,
        )
        .await;
    // The real mention proves the body was read; every other form notifies nobody.
    assert_eq!(mentions(&world, world.bob_id).await.len(), 1);
    assert!(mentions(&world, world.carol_id).await.is_empty());
    world.close().await;
}

async fn edits_notify_only_new_names(pg: bool) {
    let world = World::new(pg).await;
    let path = "/api/repos/alice/proj/issues/1";
    let created = world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Edit", "body": "Hi @bob"}),
            StatusCode::CREATED,
        )
        .await;
    let added = world
        .alice
        .expect(
            "PUT",
            path,
            json!({"expected_state": "open", "expected_updated": created["updated_at"], "body": "Hi @bob and @carol"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(mentions(&world, world.carol_id).await.len(), 1);
    // Keeping both names notifies nobody again.
    let kept = world
        .alice
        .expect(
            "PUT",
            path,
            json!({"expected_state": "open", "expected_updated": added["updated_at"], "body": "Hi @bob, @carol, thanks"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(mentions(&world, world.bob_id).await.len(), 1);
    assert_eq!(mentions(&world, world.carol_id).await.len(), 1);
    // A new name is notified. The editor, who names herself, is not.
    world
        .alice
        .expect(
            "PUT",
            path,
            json!({"expected_state": "open", "expected_updated": kept["updated_at"], "body": "Hi @bob, @carol, @dave and @alice"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(mentions(&world, world.bob_id).await.len(), 1);
    assert_eq!(mentions(&world, world.carol_id).await.len(), 1);
    assert_eq!(mentions(&world, world.dave_id).await.len(), 1);
    assert!(mentions(&world, world.alice_id).await.is_empty());
    world.close().await;
}

async fn private_repository_mentions_are_silent(pg: bool) {
    let world = World::new(pg).await;
    let secret = world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/vault/issues",
            json!({"title": "Secret", "body": "Hi @bob"}),
            StatusCode::CREATED,
        )
        .await;
    // The same request without a mention, in the public repository, has the same response shape.
    let plain = world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Plain", "body": "Hi bob"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(keys(&secret), keys(&plain));
    assert!(mentions(&world, world.bob_id).await.is_empty());
    let (status, _) = world
        .bob
        .api("GET", "/api/repos/alice/vault/issues/1", Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    world.close().await;
}

async fn mentions_are_capped_at_twenty(pg: bool) {
    let world = World::new(pg).await;
    let names: Vec<String> = (0..25).map(|index| format!("member{index:02}")).collect();
    let mut ids = Vec::new();
    for name in &names {
        ids.push(world.database.actor(name).await);
    }
    let body = names
        .iter()
        .map(|name| format!("@{name}"))
        .collect::<Vec<_>>()
        .join(" ");
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Crowd", "body": body}),
            StatusCode::CREATED,
        )
        .await;
    let mut notified = Vec::new();
    for (name, id) in names.iter().zip(&ids) {
        if !mentions(&world, *id).await.is_empty() {
            notified.push(name.clone());
        }
    }
    // The first twenty names in the text are notified, and no others.
    assert_eq!(notified, names[..20].to_vec());
    world.close().await;
}

async fn issue_author_gets_one_notice_for_a_comment(pg: bool) {
    let world = World::new(pg).await;
    world
        .bob
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Question", "body": "How?"}),
            StatusCode::CREATED,
        )
        .await;
    world
        .carol
        .expect(
            "POST",
            "/api/repos/alice/proj/issues/1/comments",
            json!({"body": "@bob see this"}),
            StatusCode::CREATED,
        )
        .await;
    let all = db::notifications::records::list_notifications_for_user(
        world.pool(),
        world.bob_id,
        100,
        false,
    )
    .await
    .unwrap();
    let kinds: Vec<&str> = all
        .iter()
        .map(|notification| notification.kind.as_str())
        .collect();
    assert_eq!(kinds, ["issue_comment"]);
    world.close().await;
}

/// An edit that newly mentions the issue author notifies them. The author got the comment's own
/// notice when it was written, and that notice is not a mention.
async fn edit_mentions_the_issue_author(pg: bool) {
    let world = World::new(pg).await;
    world
        .bob
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Question", "body": "How?"}),
            StatusCode::CREATED,
        )
        .await;
    let comment = world
        .carol
        .expect(
            "POST",
            "/api/repos/alice/proj/issues/1/comments",
            json!({"body": "Plain text"}),
            StatusCode::CREATED,
        )
        .await;
    assert!(mentions(&world, world.bob_id).await.is_empty());
    let comment_id = comment["id"].as_i64().unwrap();
    let reply = world
        .carol
        .form_reply(
            &format!("/alice/proj/issues/1/comments/{comment_id}/edit"),
            &[
                ("body", "@bob see this"),
                ("expected_body", "\"Plain text\""),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert_eq!(mentions(&world, world.bob_id).await.len(), 1);
    world.close().await;
}

/// The same for a pull request's conversation comment: an edit that newly mentions its author
/// notifies them.
async fn edit_mentions_the_pull_request_author(pg: bool) {
    let world = World::new(pg).await;
    world.alice.branch("topic", &world.base).await;
    world.alice.save("topic", "topic.txt", &world.base).await;
    world.alice.open_pull("Review me", "topic", "main").await;
    let comment = world
        .carol
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/comments",
            json!({"body": "Plain text @bob"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(mentions(&world, world.bob_id).await.len(), 1);
    assert!(mentions(&world, world.alice_id).await.is_empty());
    let comment_id = comment["id"].as_i64().unwrap();
    let reply = world
        .carol
        .form_reply(
            &format!("/alice/proj/pulls/1/comments/{comment_id}/edit"),
            &[
                ("body", "@alice see this"),
                ("expected_body", "\"Plain text @bob\""),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert_eq!(mentions(&world, world.alice_id).await.len(), 1);
    let deleted = world
        .carol
        .form_reply(
            &format!("/alice/proj/pulls/1/comments/{comment_id}/delete"),
            &[],
        )
        .await;
    assert!(deleted.status.is_redirection(), "{}", deleted.text());
    assert!(mentions(&world, world.bob_id).await.is_empty());
    assert!(mentions(&world, world.alice_id).await.is_empty());
    world.close().await;
}

async fn pull_request_mentions(pg: bool) {
    let world = World::new(pg).await;
    world.alice.branch("topic", &world.base).await;
    let head = world.alice.save("topic", "topic.txt", &world.base).await;
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls",
            json!({"title": "Review me", "body": "Please look @carol", "head_branch": "topic", "base_branch": "main"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(
        mentions(&world, world.carol_id).await,
        [(
            "alice mentioned you in alice/proj#1".to_owned(),
            Some("/alice/proj/pulls/1".to_owned())
        )]
    );

    // An edit of the body notifies the name it adds.
    let pull = world.alice.pull(1).await;
    world
        .alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/pulls/1",
            json!({"expected_state": "open", "expected_updated": pull["updated_at"], "body": "Please look @carol and @bob"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(mentions(&world, world.bob_id).await.len(), 1);
    assert_eq!(mentions(&world, world.carol_id).await.len(), 1);

    // A review body notifies the names it mentions, linking to the review.
    let review = world
        .bob
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/reviews",
            json!({"state": "commented", "body": "@dave please check"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(
        mentions(&world, world.dave_id).await,
        [(
            "bob mentioned you in alice/proj#1".to_owned(),
            Some(format!("/alice/proj/pulls/1#review-{}", review["id"]))
        )]
    );

    // An inline comment notifies the names in its text, linking to the comment.
    world
        .carol
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/comments",
            json!({"body": "@dave a nit here", "path": "topic.txt", "line": 1, "side": "RIGHT", "commit_sha": head}),
            StatusCode::CREATED,
        )
        .await;
    let links: Vec<String> = mentions(&world, world.dave_id)
        .await
        .into_iter()
        .filter_map(|(_, link)| link)
        .collect();
    assert_eq!(links.len(), 2, "{links:?}");
    assert!(
        links
            .iter()
            .any(|link| link.starts_with("/alice/proj/pulls/1?comment=")),
        "{links:?}"
    );
    world.close().await;
}

async fn native_form_issue_mentions(pg: bool) {
    let world = World::new(pg).await;
    let reply = world
        .alice
        .form_reply(
            "/alice/proj/issues/new",
            &[("title", "Form"), ("body", "Hi @bob"), ("intent", "create")],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert_eq!(mentions(&world, world.bob_id).await.len(), 1);
    world.close().await;
}

async fn restricted_accounts_do_not_use_the_budget(pg: bool) {
    let world = World::new(pg).await;
    // Sixty-one restricted names come before a real mention. Half are banned and half must
    // change their password.
    let mut restricted = Vec::new();
    let mut names = Vec::new();
    for index in 0..61 {
        let name = format!("restricted{index:02}");
        let id = world.database.actor(&name).await;
        let column = if index % 2 == 0 {
            "is_banned"
        } else {
            "must_change_password"
        };
        world
            .database
            .execute(
                &format!("UPDATE users SET {column}=TRUE WHERE id=$1"),
                &[id],
            )
            .await;
        restricted.push(id);
        names.push(format!("@{name}"));
    }
    names.push("@carol".to_owned());
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Crowd", "body": names.join(" ")}),
            StatusCode::CREATED,
        )
        .await;
    for id in &restricted {
        assert!(mentions(&world, *id).await.is_empty());
    }
    assert_eq!(mentions(&world, world.carol_id).await.len(), 1);
    world.close().await;
}

async fn unreadable_names_are_bounded_per_write(pg: bool) {
    let world = World::new(pg).await;
    let vault = db::repositories::records::get_repository(world.pool(), "alice", "vault")
        .await
        .unwrap()
        .unwrap()
        .id;
    // Bob and carol can read the private repository. Three hundred other users cannot.
    for user in [world.bob_id, world.carol_id] {
        db::repositories::access::grant_repo_access(
            world.pool(),
            user,
            vault,
            db::models::AccessLevel::Read,
        )
        .await
        .unwrap();
    }
    let mut names = vec!["@bob".to_owned()];
    for index in 0..300 {
        let name = format!("stranger{index:03}");
        world.database.actor(&name).await;
        names.push(format!("@{name}"));
    }
    // Carol comes after the first hundred names, so the write does not look her up.
    names.push("@carol".to_owned());
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/vault/issues",
            json!({"title": "Private", "body": names.join(" ")}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(mentions(&world, world.bob_id).await.len(), 1);
    assert!(mentions(&world, world.carol_id).await.is_empty());
    world.close().await;
}

async fn edit_past_the_page_cap_notifies_only_the_new_name(pg: bool) {
    let world = World::new(pg).await;
    let names: Vec<String> = (0..50).map(|index| format!("crowd{index:02}")).collect();
    let mut ids = Vec::new();
    for name in &names {
        ids.push(world.database.actor(name).await);
    }
    let body = names
        .iter()
        .map(|name| format!("@{name}"))
        .collect::<Vec<_>>()
        .join(" ");
    let created = world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Crowd", "body": body}),
            StatusCode::CREATED,
        )
        .await;
    // The edit keeps all fifty names and adds bob.
    world
        .alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/issues/1",
            json!({"expected_state": "open", "expected_updated": created["updated_at"], "body": format!("{body} @bob")}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(mentions(&world, world.bob_id).await.len(), 1);
    // Creation notified the first twenty names. The edit notifies none of the old names.
    for (index, id) in ids.iter().enumerate() {
        assert_eq!(
            mentions(&world, *id).await.len(),
            usize::from(index < 20),
            "{}",
            names[index]
        );
    }
    world.close().await;
}

#[tokio::test]
async fn postgres_restricted_accounts_do_not_use_the_budget() {
    restricted_accounts_do_not_use_the_budget(true).await;
}
#[tokio::test]
async fn sqlite_restricted_accounts_do_not_use_the_budget() {
    restricted_accounts_do_not_use_the_budget(false).await;
}

#[tokio::test]
async fn postgres_unreadable_names_are_bounded_per_write() {
    unreadable_names_are_bounded_per_write(true).await;
}
#[tokio::test]
async fn sqlite_unreadable_names_are_bounded_per_write() {
    unreadable_names_are_bounded_per_write(false).await;
}

#[tokio::test]
async fn postgres_edit_past_the_page_cap_notifies_only_the_new_name() {
    edit_past_the_page_cap_notifies_only_the_new_name(true).await;
}
#[tokio::test]
async fn sqlite_edit_past_the_page_cap_notifies_only_the_new_name() {
    edit_past_the_page_cap_notifies_only_the_new_name(false).await;
}

#[tokio::test]
async fn postgres_issue_mentions() {
    issue_mentions(true).await;
}
#[tokio::test]
async fn sqlite_issue_mentions() {
    issue_mentions(false).await;
}

#[tokio::test]
async fn postgres_deleted_comments_take_their_mention_notices() {
    deleted_comments_take_their_mention_notices(true).await;
}

#[tokio::test]
async fn sqlite_deleted_comments_take_their_mention_notices() {
    deleted_comments_take_their_mention_notices(false).await;
}

#[tokio::test]
async fn postgres_ignored_mentions() {
    ignored_mentions(true).await;
}
#[tokio::test]
async fn sqlite_ignored_mentions() {
    ignored_mentions(false).await;
}

#[tokio::test]
async fn postgres_edits_notify_only_new_names() {
    edits_notify_only_new_names(true).await;
}
#[tokio::test]
async fn sqlite_edits_notify_only_new_names() {
    edits_notify_only_new_names(false).await;
}

#[tokio::test]
async fn postgres_private_repository_mentions_are_silent() {
    private_repository_mentions_are_silent(true).await;
}
#[tokio::test]
async fn sqlite_private_repository_mentions_are_silent() {
    private_repository_mentions_are_silent(false).await;
}

#[tokio::test]
async fn postgres_mentions_are_capped_at_twenty() {
    mentions_are_capped_at_twenty(true).await;
}
#[tokio::test]
async fn sqlite_mentions_are_capped_at_twenty() {
    mentions_are_capped_at_twenty(false).await;
}

#[tokio::test]
async fn postgres_issue_author_gets_one_notice_for_a_comment() {
    issue_author_gets_one_notice_for_a_comment(true).await;
}
#[tokio::test]
async fn sqlite_issue_author_gets_one_notice_for_a_comment() {
    issue_author_gets_one_notice_for_a_comment(false).await;
}

#[tokio::test]
async fn postgres_edit_mentions_the_issue_author() {
    edit_mentions_the_issue_author(true).await;
}
#[tokio::test]
async fn sqlite_edit_mentions_the_issue_author() {
    edit_mentions_the_issue_author(false).await;
}

#[tokio::test]
async fn postgres_edit_mentions_the_pull_request_author() {
    edit_mentions_the_pull_request_author(true).await;
}
#[tokio::test]
async fn sqlite_edit_mentions_the_pull_request_author() {
    edit_mentions_the_pull_request_author(false).await;
}

#[tokio::test]
async fn postgres_pull_request_mentions() {
    pull_request_mentions(true).await;
}
#[tokio::test]
async fn sqlite_pull_request_mentions() {
    pull_request_mentions(false).await;
}

#[tokio::test]
async fn postgres_native_form_issue_mentions() {
    native_form_issue_mentions(true).await;
}
#[tokio::test]
async fn sqlite_native_form_issue_mentions() {
    native_form_issue_mentions(false).await;
}

/// A new issue notifies "All activity" watchers and "Custom" watchers who picked issues. An
/// ignoring user is told nothing, even when mentioned.
async fn watch_levels(pg: bool) {
    let world = World::new(pg).await;
    let proj = db::repositories::records::get_repository(world.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    for (user, level, events) in [
        (world.bob_id, "watching", ""),
        (world.carol_id, "custom", "pulls"),
        (world.dave_id, "ignoring", ""),
    ] {
        db::repositories::engagement::set_watch_level(world.pool(), user, proj.id, level, events)
            .await
            .unwrap();
    }
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Watched", "body": "Ping @bob and @dave"}),
            StatusCode::CREATED,
        )
        .await;
    let opened = notices(&world, world.bob_id, "issue_opened").await;
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].title, "alice opened issue #1 in alice/proj");
    // The watcher is told once, not again for the mention.
    assert!(mentions(&world, world.bob_id).await.is_empty());
    assert!(notices(&world, world.carol_id, "issue_opened")
        .await
        .is_empty());
    assert!(notices(&world, world.dave_id, "issue_opened")
        .await
        .is_empty());
    assert!(mentions(&world, world.dave_id).await.is_empty());
    world.close().await;
}

#[tokio::test]
async fn postgres_watch_levels() {
    watch_levels(true).await;
}
#[tokio::test]
async fn sqlite_watch_levels() {
    watch_levels(false).await;
}

/// Watchers past the first batch of a repository's watch list still hear about new issues, and
/// a banned watcher hears nothing.
async fn watchers_past_the_first_batch_are_notified(pg: bool) {
    let world = World::new(pg).await;
    let proj = db::repositories::records::get_repository(world.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    // Two hundred custom watchers who picked pull requests come before the one who picked issues.
    for index in 0..200 {
        let user = db::users::accounts::create_user(world.pool(), &format!("pulls{index}"), None)
            .await
            .unwrap();
        db::repositories::engagement::set_watch_level(
            world.pool(),
            user.id,
            proj.id,
            "custom",
            "pulls",
        )
        .await
        .unwrap();
    }
    let late = db::users::accounts::create_user(world.pool(), "lateissues", None)
        .await
        .unwrap();
    db::repositories::engagement::set_watch_level(
        world.pool(),
        late.id,
        proj.id,
        "custom",
        "issues",
    )
    .await
    .unwrap();
    // Two hundred "All activity" watchers come after it, so the reads take a second batch.
    let mut last_watcher = None;
    for index in 0..200 {
        let user =
            db::users::accounts::create_user(world.pool(), &format!("watching{index}"), None)
                .await
                .unwrap();
        db::repositories::engagement::set_watch_level(
            world.pool(),
            user.id,
            proj.id,
            "watching",
            "",
        )
        .await
        .unwrap();
        last_watcher = Some(user.id);
    }
    let banned = db::users::accounts::create_user(world.pool(), "bannedwatch", None)
        .await
        .unwrap();
    db::repositories::engagement::set_watch_level(world.pool(), banned.id, proj.id, "watching", "")
        .await
        .unwrap();
    world
        .database
        .execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[banned.id])
        .await;
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Late", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(notices(&world, late.id, "issue_opened").await.len(), 1);
    let last_watcher = last_watcher.expect("the loop ran");
    assert_eq!(
        notices(&world, last_watcher, "issue_opened").await.len(),
        1,
        "the second batch is notified too"
    );
    assert!(notices(&world, banned.id, "issue_opened").await.is_empty());
    world.close().await;
}

#[tokio::test]
async fn postgres_watchers_past_the_first_batch_are_notified() {
    watchers_past_the_first_batch_are_notified(true).await;
}
#[tokio::test]
async fn sqlite_watchers_past_the_first_batch_are_notified() {
    watchers_past_the_first_batch_are_notified(false).await;
}
