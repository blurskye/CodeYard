//! Pull request review rules: who may review, when a review needs a body, how approvals
//! and change requests count under a branch rule, reply threading and inline comment
//! listing, on SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

#[path = "support/fake_webui_bundle.rs"]
mod bundle;
#[path = "support/database.rs"]
mod database;
#[path = "editor_save/fixture.rs"]
mod fixture;
#[path = "support/native_database.rs"]
mod native;
#[path = "support/production_ci.rs"]
mod production;
#[path = "support/pull_requests.rs"]
mod pulls;

use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use database::Database;
use pulls::{text, Client};
use serde_json::{json, Value};
use std::sync::Arc;

/// `alice/proj`, a private repository whose `main` holds one commit. Bob and Carol have
/// write access, Dave can only read, and Erin has no access.
struct World {
    database: Database,
    _root: tempfile::TempDir,
    repo_id: i64,
    base: String,
    alice_id: i64,
    alice: Client,
    bob: Client,
    carol: Client,
    dave: Client,
    erin: Client,
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
        let erin_id = database.actor("erin").await;
        let root = tempfile::tempdir().unwrap();
        let repository = codeyard::backend::repositories::lifecycle::provision(
            (database.pool(), None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
            "proj",
            None,
            true,
            None,
        )
        .await
        .unwrap();
        for (user, level) in [
            (bob_id, db::models::AccessLevel::Write),
            (carol_id, db::models::AccessLevel::Write),
            (dave_id, db::models::AccessLevel::Read),
        ] {
            db::repositories::access::grant_repo_access(
                database.pool(),
                user,
                repository.id,
                level,
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
        let dave = Client::login(&router, database.pool(), dave_id).await;
        let erin = Client::login(&router, database.pool(), erin_id).await;
        let base = alice.save("main", "README.md", "unborn").await;
        Self {
            database,
            _root: root,
            repo_id: repository.id,
            base,
            alice_id,
            alice,
            bob,
            carol,
            dave,
            erin,
        }
    }
}

fn reviews(number: i64) -> String {
    format!("/api/repos/alice/proj/pulls/{number}/reviews")
}

/// Opens a pull request from a new branch that adds one file to `base`, and returns its
/// number and head commit.
async fn open_pull(alice: &Client, branch: &str, base: &str) -> (i64, String) {
    alice.branch(branch, base).await;
    let head = alice.save(branch, &format!("{branch}.txt"), base).await;
    let pull = alice.open_pull(branch, branch, "main").await;
    (pull["number"].as_i64().unwrap(), head)
}

/// Sets the only rule on `main`: pull requests required, one approval, and the given
/// stale-approval setting.
async fn protect(alice: &Client, dismiss_stale_reviews: bool) {
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({
                "pattern": "main",
                "require_pull_request": true,
                "required_approving_reviews": 1,
                "dismiss_stale_reviews": dismiss_stale_reviews
            }),
            StatusCode::OK,
        )
        .await;
}

/// A merge that the branch rules refuse, with the reason in the message.
async fn refused_merge(alice: &Client, number: i64, reason: &str) {
    let pull = alice.pull(number).await;
    let error = alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/merge"),
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"]}),
            StatusCode::CONFLICT,
        )
        .await;
    assert_eq!(error["error"], "branch_protection", "{error}");
    assert!(text(&error, "message").contains(reason), "{error}");
    assert_eq!(alice.pull(number).await["state"], "open");
}

async fn permissions_and_bodies_contract(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", &world.base).await;
    let path = reviews(number);

    // A reader may leave a plain comment review, but not vote.
    let comment = world
        .dave
        .expect(
            "POST",
            &path,
            json!({"state": "commented", "body": "Reading along"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(comment["state"], "commented", "{comment}");
    for state in ["approved", "changes_requested"] {
        let (status, refused) = world
            .dave
            .api(
                "POST",
                &path,
                json!({"state": state, "body": "Vote", "commit_sha": head}),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{state}: {refused}");
        assert_eq!(refused["error"], "not_found", "{refused}");
    }
    // Outsiders of a private repository cannot review at all.
    let (status, refused) = world
        .erin
        .api("POST", &path, json!({"body": "Hello"}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");

    // An approval may stand alone; comments and change requests must say why.
    world
        .bob
        .expect(
            "POST",
            &path,
            json!({"state": "approved", "commit_sha": head}),
            StatusCode::CREATED,
        )
        .await;
    for body in [
        json!({"state": "commented"}),
        json!({"state": "commented", "body": "   "}),
    ] {
        let (status, refused) = world.bob.api("POST", &path, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
        assert_eq!(refused["error"], "validation_error", "{refused}");
    }
    let (status, refused) = world
        .bob
        .api("POST", &path, json!({"state": "changes_requested"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"], "validation_error", "{refused}");

    // The author may comment on their own pull request but cannot vote on it.
    world
        .alice
        .expect(
            "POST",
            &path,
            json!({"state": "commented", "body": "Ready for review"}),
            StatusCode::CREATED,
        )
        .await;
    for (state, message) in [
        ("approved", "Authors cannot approve their own pull request"),
        (
            "changes_requested",
            "Authors cannot request changes on their own pull request",
        ),
    ] {
        let (status, refused) = world
            .alice
            .api(
                "POST",
                &path,
                json!({"state": state, "body": "Mine", "commit_sha": head}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{state}: {refused}");
        assert_eq!(refused["error"], "validation_error", "{refused}");
        assert_eq!(text(&refused, "message"), message);
    }

    // Only the three accepted reviews were stored.
    let stored = world
        .alice
        .expect("GET", &path, Value::Null, StatusCode::OK)
        .await;
    let states: Vec<&str> = stored
        .as_array()
        .unwrap()
        .iter()
        .map(|review| review["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["commented", "approved", "commented"], "{stored}");
    world.database.finish().await;
}

async fn stale_approvals_contract(pg: bool) {
    let mut world = World::new(pg).await;
    protect(&world.alice, true).await;
    let (number, first) = open_pull(&world.alice, "topic", &world.base).await;
    world
        .bob
        .expect(
            "POST",
            &reviews(number),
            json!({"state": "approved", "commit_sha": first}),
            StatusCode::CREATED,
        )
        .await;
    // The branch moves, so the approval names an earlier commit.
    world.alice.save("topic", "second.txt", &first).await;
    refused_merge(&world.alice, number, "1 approving review required, found 0").await;

    // With stale approvals allowed, the approval counts for any commit.
    protect(&world.alice, false).await;
    let merged = world.alice.merge_current(number).await;
    assert_eq!(merged["merged"], true, "{merged}");
    world.database.finish().await;
}

async fn changes_requested_contract(pg: bool) {
    let mut world = World::new(pg).await;
    protect(&world.alice, true).await;

    // A change request holds until its author dismisses it, whatever commit it names.
    let (number, first) = open_pull(&world.alice, "dismissed", &world.base).await;
    world
        .carol
        .expect(
            "POST",
            &reviews(number),
            json!({"state": "approved", "commit_sha": first}),
            StatusCode::CREATED,
        )
        .await;
    let changes = world
        .bob
        .expect(
            "POST",
            &reviews(number),
            json!({"state": "changes_requested", "body": "Rename it", "commit_sha": first}),
            StatusCode::CREATED,
        )
        .await;
    refused_merge(&world.alice, number, "changes_requested").await;
    let second = world.alice.save("dismissed", "again.txt", &first).await;
    refused_merge(&world.alice, number, "changes_requested").await;
    world
        .alice
        .expect(
            "POST",
            &format!(
                "{}/{}/dismiss",
                reviews(number),
                changes["id"].as_i64().unwrap()
            ),
            json!({"reason": "Addressed"}),
            StatusCode::OK,
        )
        .await;
    // Carol's approval names the old commit, so it no longer counts.
    refused_merge(&world.alice, number, "1 approving review required, found 0").await;
    world
        .carol
        .expect(
            "POST",
            &reviews(number),
            json!({"state": "approved", "commit_sha": second}),
            StatusCode::CREATED,
        )
        .await;
    let merged = world.alice.merge_current(number).await;
    assert_eq!(merged["merged"], true, "{merged}");
    let tip = text(&merged, "sha").to_owned();

    // A new vote by the same reviewer replaces the change request.
    let (number, first) = open_pull(&world.alice, "rereview", &tip).await;
    world
        .bob
        .expect(
            "POST",
            &reviews(number),
            json!({"state": "changes_requested", "body": "Split it", "commit_sha": first}),
            StatusCode::CREATED,
        )
        .await;
    // The first pull request already merged its again.txt into main.
    let second = world.alice.save("rereview", "followup.txt", &first).await;
    refused_merge(&world.alice, number, "changes_requested").await;
    world
        .bob
        .expect(
            "POST",
            &reviews(number),
            json!({"state": "approved", "commit_sha": second}),
            StatusCode::CREATED,
        )
        .await;
    let merged = world.alice.merge_current(number).await;
    assert_eq!(merged["merged"], true, "{merged}");
    world.database.finish().await;
}

async fn threads_and_inline_listing_contract(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, first) = open_pull(&world.alice, "threads", &world.base).await;
    let comments = format!("/api/repos/alice/proj/pulls/{number}/comments");
    let root = world
        .alice
        .expect(
            "POST",
            &comments,
            json!({"body": "Check this", "path": "threads.txt", "line": 1, "side": "RIGHT"}),
            StatusCode::CREATED,
        )
        .await;
    let root_id = root["id"].as_i64().unwrap();
    let reply = world
        .bob
        .expect(
            "POST",
            &comments,
            json!({"body": "Agreed", "in_reply_to_comment_id": root_id}),
            StatusCode::CREATED,
        )
        .await;
    let reply_id = reply["id"].as_i64().unwrap();
    assert_eq!(reply["in_reply_to_comment_id"], root_id, "{reply}");
    assert_eq!(reply["path"], "threads.txt", "{reply}");
    // A reply to a reply is stored under the thread root.
    let second = world
        .alice
        .expect(
            "POST",
            &comments,
            json!({"body": "Thanks", "in_reply_to_comment_id": reply_id}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(second["in_reply_to_comment_id"], root_id, "{second}");
    // A reply saved before normalization sits under another reply; a new reply to it
    // still joins the root.
    let legacy = db::pull_requests::reviews::add_pull_request_comment(
        world.database.pool(),
        db::pull_requests::reviews::NewPullRequestComment {
            pr_id: pull_row_id(&world, number).await,
            author_id: world.alice_id,
            body: "Legacy",
            path: Some("threads.txt"),
            line: Some(1),
            side: Some("RIGHT"),
            commit_sha: Some(first.as_str()),
            in_reply_to_comment_id: Some(reply_id),
        },
    )
    .await
    .unwrap();
    let answered = world
        .carol
        .expect(
            "POST",
            &comments,
            json!({"body": "Answer", "in_reply_to_comment_id": legacy.id}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(answered["in_reply_to_comment_id"], root_id, "{answered}");
    world
        .alice
        .expect(
            "POST",
            &comments,
            json!({"body": "General discussion"}),
            StatusCode::CREATED,
        )
        .await;

    // Only comments on files are listed inline, oldest first.
    let pr_id = pull_row_id(&world, number).await;
    let listed =
        db::pull_requests::reviews::list_pull_request_inline_comments(world.database.pool(), pr_id)
            .await
            .unwrap();
    assert!(!listed.truncated);
    let ids: Vec<i64> = listed
        .comments
        .iter()
        .map(|entry| entry.comment.id)
        .collect();
    assert_eq!(
        ids,
        [
            root_id,
            reply_id,
            second["id"].as_i64().unwrap(),
            legacy.id,
            answered["id"].as_i64().unwrap()
        ]
    );

    // The listing stops at 500 and says that more exist.
    for n in 5..500 {
        db::pull_requests::reviews::add_pull_request_comment(
            world.database.pool(),
            db::pull_requests::reviews::NewPullRequestComment {
                pr_id,
                author_id: world.alice_id,
                body: &format!("Filler {n}"),
                path: Some("threads.txt"),
                line: Some(1),
                side: Some("RIGHT"),
                commit_sha: Some(first.as_str()),
                in_reply_to_comment_id: None,
            },
        )
        .await
        .unwrap();
    }
    let full =
        db::pull_requests::reviews::list_pull_request_inline_comments(world.database.pool(), pr_id)
            .await
            .unwrap();
    assert_eq!(full.comments.len(), 500);
    assert!(!full.truncated);
    db::pull_requests::reviews::add_pull_request_comment(
        world.database.pool(),
        db::pull_requests::reviews::NewPullRequestComment {
            pr_id,
            author_id: world.alice_id,
            body: "One too many",
            path: Some("threads.txt"),
            line: Some(1),
            side: Some("RIGHT"),
            commit_sha: Some(first.as_str()),
            in_reply_to_comment_id: None,
        },
    )
    .await
    .unwrap();
    let capped =
        db::pull_requests::reviews::list_pull_request_inline_comments(world.database.pool(), pr_id)
            .await
            .unwrap();
    assert_eq!(capped.comments.len(), 500);
    assert!(capped.truncated);
    world.database.finish().await;
}

/// The database id of pull request `number` in `alice/proj`.
async fn pull_row_id(world: &World, number: i64) -> i64 {
    db::pull_requests::records::get_pull_request(
        world.database.pool(),
        world.repo_id,
        number as i32,
    )
    .await
    .unwrap()
    .unwrap()
    .id
}

#[tokio::test]
async fn sqlite_review_permissions_and_bodies_follow_the_rules() {
    permissions_and_bodies_contract(false).await;
}

#[tokio::test]
async fn postgres_review_permissions_and_bodies_follow_the_rules() {
    permissions_and_bodies_contract(true).await;
}

#[tokio::test]
async fn sqlite_stale_approvals_follow_the_stale_review_setting() {
    stale_approvals_contract(false).await;
}

#[tokio::test]
async fn postgres_stale_approvals_follow_the_stale_review_setting() {
    stale_approvals_contract(true).await;
}

#[tokio::test]
async fn sqlite_changes_requested_block_until_dismissed_or_reviewed_again() {
    changes_requested_contract(false).await;
}

#[tokio::test]
async fn postgres_changes_requested_block_until_dismissed_or_reviewed_again() {
    changes_requested_contract(true).await;
}

#[tokio::test]
async fn sqlite_replies_join_the_root_and_inline_listing_is_capped() {
    threads_and_inline_listing_contract(false).await;
}

#[tokio::test]
async fn postgres_replies_join_the_root_and_inline_listing_is_capped() {
    threads_and_inline_listing_contract(true).await;
}

/// Legacy rows can make comments reply to each other in a cycle, or to themselves. A reply to
/// such a comment is refused and stores nothing; a healthy chain still joins its root.
async fn damaged_threads_contract(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, _) = open_pull(&world.alice, "damaged", &world.base).await;
    let comments = format!("/api/repos/alice/proj/pulls/{number}/comments");
    let pr_id = pull_row_id(&world, number).await;

    let mut ids = Vec::new();
    for body in ["First", "Second", "Lone"] {
        let comment = world
            .alice
            .expect(
                "POST",
                &comments,
                json!({"body": body, "path": "damaged.txt", "line": 1, "side": "RIGHT"}),
                StatusCode::CREATED,
            )
            .await;
        ids.push(comment["id"].as_i64().unwrap());
    }
    let (first_id, second_id, lone_id) = (ids[0], ids[1], ids[2]);
    world
        .database
        .execute(
            &format!(
                "UPDATE pr_comments SET in_reply_to_comment_id={second_id} WHERE id={first_id}"
            ),
            &[],
        )
        .await;
    world
        .database
        .execute(
            &format!(
                "UPDATE pr_comments SET in_reply_to_comment_id={first_id} WHERE id={second_id}"
            ),
            &[],
        )
        .await;
    world
        .database
        .execute(
            &format!("UPDATE pr_comments SET in_reply_to_comment_id={lone_id} WHERE id={lone_id}"),
            &[],
        )
        .await;

    let before =
        db::pull_requests::reviews::count_pull_request_comments(world.database.pool(), pr_id)
            .await
            .unwrap();
    for parent in [first_id, second_id, lone_id] {
        let (status, refused) = world
            .bob
            .api(
                "POST",
                &comments,
                json!({"body": "Reply", "in_reply_to_comment_id": parent}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
        assert_eq!(refused["error"], "validation_error", "{refused}");
        assert_eq!(
            text(&refused, "message"),
            "That comment thread is damaged and cannot be replied to."
        );
    }
    let after =
        db::pull_requests::reviews::count_pull_request_comments(world.database.pool(), pr_id)
            .await
            .unwrap();
    assert_eq!(after, before);

    let root = world
        .alice
        .expect(
            "POST",
            &comments,
            json!({"body": "Root", "path": "damaged.txt", "line": 1, "side": "RIGHT"}),
            StatusCode::CREATED,
        )
        .await;
    let root_id = root["id"].as_i64().unwrap();
    let reply = world
        .bob
        .expect(
            "POST",
            &comments,
            json!({"body": "Reply", "in_reply_to_comment_id": root_id}),
            StatusCode::CREATED,
        )
        .await;
    let nested = world
        .alice
        .expect(
            "POST",
            &comments,
            json!({"body": "Nested", "in_reply_to_comment_id": reply["id"]}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(nested["in_reply_to_comment_id"], root_id, "{nested}");
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_damaged_reply_threads_are_refused_and_healthy_ones_join_the_root() {
    damaged_threads_contract(false).await;
}

#[tokio::test]
async fn postgres_damaged_reply_threads_are_refused_and_healthy_ones_join_the_root() {
    damaged_threads_contract(true).await;
}
