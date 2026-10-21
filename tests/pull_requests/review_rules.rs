//! Pull request review rules: who may review, when a review needs a body, how approvals
//! and change requests count under a branch rule, reply threading and inline comment
//! listing, on SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client as pulls;

use axum::http::StatusCode;
use codeyard::backend::db;
use database::Database;
use pulls::{text, Client};
use serde_json::{json, Value};

/// `alice/proj`, a private repository whose `main` holds one commit. Bob and Carol have
/// write access, Dave can only read, and Erin has no access.
struct World {
    database: Database,
    _root: tempfile::TempDir,
    repo_id: i64,
    base: String,
    alice_id: i64,
    bob_id: i64,
    carol_id: i64,
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
        database.complete_setup().await;
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
        let router = database.router(root.path()).await;
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
            bob_id,
            carol_id,
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
    // A reply can sit under another reply. A new reply to it still joins the root.
    let nested = db::pull_requests::reviews::add_pull_request_comment(
        world.database.pool(),
        db::pull_requests::reviews::NewPullRequestComment {
            pr_id: pull_row_id(&world, number).await,
            author_id: world.alice_id,
            body: "Nested",
            expected_revisions: None,
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
            json!({"body": "Answer", "in_reply_to_comment_id": nested.id}),
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
            nested.id,
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
                expected_revisions: None,
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
            expected_revisions: None,
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

/// A reply under another reply joins the root of its thread.
async fn nested_replies_join_the_root_contract(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, _) = open_pull(&world.alice, "nested", &world.base).await;
    let comments = format!("/api/repos/alice/proj/pulls/{number}/comments");

    let root = world
        .alice
        .expect(
            "POST",
            &comments,
            json!({"body": "Root", "path": "nested.txt", "line": 1, "side": "RIGHT"}),
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
async fn sqlite_nested_replies_join_the_root() {
    nested_replies_join_the_root_contract(false).await;
}

#[tokio::test]
async fn postgres_nested_replies_join_the_root() {
    nested_replies_join_the_root_contract(true).await;
}

/// A loop in the reply chain is refused. A reply to a comment whose chain points back into
/// itself, either to itself or through another comment, is a validation error and stores
/// nothing.
async fn reply_chain_loop_contract(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, _) = open_pull(&world.alice, "loops", &world.base).await;
    let comments = format!("/api/repos/alice/proj/pulls/{number}/comments");
    let pr_id = pull_row_id(&world, number).await;

    let mut ids = Vec::new();
    for body in ["First", "Second", "Lone"] {
        let comment = world
            .alice
            .expect(
                "POST",
                &comments,
                json!({"body": body, "path": "loops.txt", "line": 1, "side": "RIGHT"}),
                StatusCode::CREATED,
            )
            .await;
        ids.push(comment["id"].as_i64().unwrap());
    }
    let (first_id, second_id, lone_id) = (ids[0], ids[1], ids[2]);
    // First and Second reply to each other, and Lone replies to itself.
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
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_a_loop_in_the_reply_chain_is_refused() {
    reply_chain_loop_contract(false).await;
}

#[tokio::test]
async fn postgres_a_loop_in_the_reply_chain_is_refused() {
    reply_chain_loop_contract(true).await;
}

fn web_pull(number: i64) -> String {
    format!("/alice/proj/pulls/{number}")
}

/// Bob files a comment on the first line of `topic.txt` in his pending review.
async fn start_pending_comment(bob: &Client, page: &str, head: &str, body: &str) -> pulls::Reply {
    let number = page.rsplit('/').next().unwrap().parse().unwrap();
    let pull = bob.pull(number).await;
    bob.form_reply(
        &format!("{page}/comments"),
        &[
            ("body", body),
            ("path", "topic.txt"),
            ("side", "RIGHT"),
            ("line", "1"),
            ("commit_sha", head),
            ("base_sha", pull["base_sha"].as_str().unwrap()),
            ("from", "diff"),
            ("intent", "review"),
        ],
    )
    .await
}

#[tokio::test]
async fn sqlite_committed_review_succeeds_when_notification_comments_cannot_be_read() {
    let mut world = World::new(false).await;
    let (number, head) = open_pull(&world.alice, "topic", &world.base).await;
    let started =
        start_pending_comment(&world.bob, &web_pull(number), &head, "Pending thought").await;
    assert!(started.status.is_redirection(), "{}", started.text());
    // A malformed stored timestamp makes the optional post-submit comment read fail decoding.
    world
        .database
        .execute("UPDATE pr_comments SET created_at = 'invalid'", &[])
        .await;
    let review = world
        .bob
        .expect(
            "POST",
            &reviews(number),
            json!({"state": "approved"}),
            StatusCode::CREATED,
        )
        .await;
    let saved = db::pull_requests::reviews::get_pull_request_review_by_id(
        world.database.pool(),
        review["id"].as_i64().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(saved.state, "approved");
    world.database.finish().await;
}

/// A pending review is its author's alone. Its comment reaches the discussion only when the
/// review is submitted, and then the comment sits under the review in the Conversation.
async fn pending_review_is_private_until_submitted(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web_pull(number);
    let diff = format!("{page}/diff");

    let started = start_pending_comment(&world.bob, &page, &head, "Pending thought").await;
    assert!(started.status.is_redirection(), "{}", started.text());

    // Bob sees his comment on the diff, marked pending, with the count on the review menu.
    let (_, bob_files) = world.bob.page(&diff).await;
    assert!(bob_files.contains("Pending thought"));
    assert!(bob_files.contains(">Pending</span>"));
    assert!(bob_files.contains("Discard review"));
    // Nobody else sees it: not the author, not a reader, and not the API.
    for viewer in [&world.alice, &world.dave] {
        let files = viewer.page_ok(&diff).await;
        assert!(!files.contains("Pending thought"));
        let conversation = viewer.page_ok(&page).await;
        assert!(!conversation.contains("Pending thought"));
    }
    let (status, listed) = world
        .alice
        .api(
            "GET",
            &format!("/api/repos/alice/proj/pulls/{number}/comments"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(!listed.to_string().contains("Pending thought"), "{listed}");

    // Submitting publishes the comment with the review, which lists it under its state.
    let submitted = world
        .bob
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "commented"),
                ("body", "Overall fine"),
                ("commit_sha", head.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert!(submitted.status.is_redirection(), "{}", submitted.text());
    assert!(
        submitted
            .location
            .as_deref()
            .is_some_and(|location| location.contains("#review-")),
        "{:?}",
        submitted.location
    );
    let (_, alice_page) = world.alice.page(&page).await;
    assert!(alice_page.contains("Commented"));
    let review_at = alice_page.find("id=\"review-").unwrap();
    let comment_at = alice_page.find("Pending thought").unwrap();
    assert!(review_at < comment_at, "the comment sits under its review");
    assert!(!alice_page[review_at..comment_at].contains("</article>"));
    assert!(!alice_page.contains(">Pending</span>"));
    // Bob's review menu no longer offers to discard anything.
    let bob_files = world.bob.page_ok(&diff).await;
    assert!(!bob_files.contains("Discard review"));
    assert!(!bob_files.contains(">Pending</span>"));

    world.database.finish().await;
}

/// Discarding removes the pending review and its comments. Nothing is left to discard after that.
async fn discarding_a_pending_review_removes_its_comments(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web_pull(number);
    let diff = format!("{page}/diff");

    let started = start_pending_comment(&world.bob, &page, &head, "Discard me").await;
    assert!(started.status.is_redirection(), "{}", started.text());
    let discarded = world
        .bob
        .form_reply(&format!("{page}/reviews/pending/discard"), &[])
        .await;
    assert!(discarded.status.is_redirection(), "{}", discarded.text());
    assert!(
        discarded
            .location
            .as_deref()
            .is_some_and(|location| location.ends_with("/diff")),
        "{:?}",
        discarded.location
    );
    let bob_files = world.bob.page_ok(&diff).await;
    assert!(!bob_files.contains("Discard me"));
    assert!(!bob_files.contains("Discard review"));

    let again = world
        .bob
        .form_reply(&format!("{page}/reviews/pending/discard"), &[])
        .await;
    assert!(again.status.is_client_error(), "{}", again.text());

    world.database.finish().await;
}

/// The author may comment on their pull request, but a review that approves it or asks for changes
/// is refused on the Files changed page. A comment review needs a summary unless it has inline
/// comments of its own.
async fn author_and_summary_rules_for_submitted_reviews(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web_pull(number);

    let approved = world
        .alice
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "approved"),
                ("body", ""),
                ("commit_sha", head.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert_eq!(
        approved.status,
        StatusCode::BAD_REQUEST,
        "{}",
        approved.text()
    );
    assert!(approved
        .text()
        .contains("Authors cannot approve their own pull request"));

    let empty = world
        .bob
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "commented"),
                ("body", ""),
                ("commit_sha", head.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{}", empty.text());
    assert!(empty.text().contains("A review body is required"));

    // Inline comments in the pending review stand in for the summary.
    let started = start_pending_comment(&world.bob, &page, &head, "Inline only").await;
    assert!(started.status.is_redirection(), "{}", started.text());
    let submitted = world
        .bob
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "commented"),
                ("body", ""),
                ("commit_sha", head.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert!(submitted.status.is_redirection(), "{}", submitted.text());
    let (_, alice_page) = world.alice.page(&page).await;
    assert!(alice_page.contains("Inline only"));

    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_pending_review_is_private_until_submitted() {
    pending_review_is_private_until_submitted(false).await;
}

#[tokio::test]
async fn postgres_pending_review_is_private_until_submitted() {
    pending_review_is_private_until_submitted(true).await;
}

#[tokio::test]
async fn sqlite_discarding_a_pending_review_removes_its_comments() {
    discarding_a_pending_review_removes_its_comments(false).await;
}

#[tokio::test]
async fn postgres_discarding_a_pending_review_removes_its_comments() {
    discarding_a_pending_review_removes_its_comments(true).await;
}

#[tokio::test]
async fn sqlite_author_and_summary_rules_for_submitted_reviews() {
    author_and_summary_rules_for_submitted_reviews(false).await;
}

#[tokio::test]
async fn postgres_author_and_summary_rules_for_submitted_reviews() {
    author_and_summary_rules_for_submitted_reviews(true).await;
}

/// The refusal a database write failed with, if it was one.
fn refusal(error: &anyhow::Error) -> Option<db::discussions::DiscussionWriteRefusal> {
    error
        .downcast_ref::<db::discussions::DiscussionWriteRefusal>()
        .copied()
}

/// A pending inline comment from `author_id` on the first line of `topic.txt`, filed through the
/// database.
fn pending_input<'a>(
    pr_id: i64,
    author_id: i64,
    head: &'a str,
    body: &'a str,
) -> db::pull_requests::reviews::NewPullRequestComment<'a> {
    db::pull_requests::reviews::NewPullRequestComment {
        pr_id,
        author_id,
        body,
        expected_revisions: None,
        path: Some("topic.txt"),
        line: Some(1),
        side: Some("RIGHT"),
        commit_sha: Some(head),
        in_reply_to_comment_id: None,
    }
}

/// The mention notifications a user holds.
async fn mention_count(world: &World, user: i64) -> usize {
    notification_links(world, user, "mention").await.len()
}

/// The links of a user's notifications of one kind.
async fn notification_links(world: &World, user: i64, kind: &str) -> Vec<String> {
    db::notifications::records::list_notifications_for_user(world.database.pool(), user, 100, false)
        .await
        .unwrap()
        .into_iter()
        .filter(|notification| notification.kind == kind)
        .filter_map(|notification| notification.link)
        .collect()
}

/// A reply inside the author's pending thread waits with the thread. Its mention reaches the other
/// user when the review is submitted, not before.
async fn pending_reply_waits_for_submit(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let comments = format!("/api/repos/alice/proj/pulls/{number}/comments");
    let root = world
        .bob
        .expect(
            "POST",
            &comments,
            json!({"body": "Pending root", "path": "topic.txt", "line": 1, "side": "RIGHT", "commit_sha": head, "pending": true}),
            StatusCode::CREATED,
        )
        .await;
    world
        .bob
        .expect(
            "POST",
            &comments,
            json!({"body": "@carol a pending reply", "in_reply_to_comment_id": root["id"]}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(mention_count(&world, world.carol_id).await, 0);
    world
        .bob
        .expect(
            "POST",
            &reviews(number),
            json!({"state": "commented", "body": "Done", "commit_sha": head}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(mention_count(&world, world.carol_id).await, 1);
    world.database.finish().await;
}

/// A pending review holds at most `MAX_REVIEW_COMMENTS` comments. The write that would add one more
/// is refused, and the review keeps the comments it had.
async fn pending_review_holds_at_most_200_comments(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let pr_id = pull_row_id(&world, number).await;
    let pool = world.database.pool();
    for index in 0..db::pull_requests::reviews::MAX_REVIEW_COMMENTS {
        let body = format!("Comment {index}");
        db::pull_requests::reviews::add_pending_pull_request_comment(
            pool,
            pending_input(pr_id, world.bob_id, &head, &body),
        )
        .await
        .unwrap();
    }
    let error = db::pull_requests::reviews::add_pending_pull_request_comment(
        pool,
        pending_input(pr_id, world.bob_id, &head, "One too many"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Some(db::discussions::DiscussionWriteRefusal::ReviewFull)
        ),
        "{error:#}"
    );
    let held = db::pull_requests::reviews::list_pending_review_comments(pool, pr_id, world.bob_id)
        .await
        .unwrap();
    assert_eq!(held.len(), 200);
    world.database.finish().await;
}

/// The pending review's preconditions are checked in the write itself. A pending comment does not
/// start on a closed pull request, and a review without a summary is not submitted once its inline
/// comments have been discarded.
async fn writes_recheck_the_pull_request_and_the_pending_review(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let pr_id = pull_row_id(&world, number).await;
    let pool = world.database.pool();

    world
        .database
        .execute(
            "UPDATE pull_requests SET state = 'closed' WHERE id = $1",
            &[pr_id],
        )
        .await;
    let error = db::pull_requests::reviews::add_pending_pull_request_comment(
        pool,
        pending_input(pr_id, world.bob_id, &head, "Too late"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Some(db::discussions::DiscussionWriteRefusal::InvalidReview)
        ),
        "{error:#}"
    );
    assert!(
        db::pull_requests::reviews::get_pending_review(pool, pr_id, world.bob_id)
            .await
            .unwrap()
            .is_none()
    );

    world
        .database
        .execute(
            "UPDATE pull_requests SET state = 'open' WHERE id = $1",
            &[pr_id],
        )
        .await;
    db::pull_requests::reviews::add_pending_pull_request_comment(
        pool,
        pending_input(pr_id, world.bob_id, &head, "Inline only"),
    )
    .await
    .unwrap();
    assert!(
        db::pull_requests::reviews::discard_pending_review(pool, pr_id, world.bob_id)
            .await
            .unwrap()
    );
    let error = db::pull_requests::reviews::add_pull_request_review(
        pool,
        pr_id,
        world.bob_id,
        "commented",
        None,
        Some(head.as_str()),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Some(db::discussions::DiscussionWriteRefusal::InvalidReview)
        ),
        "{error:#}"
    );
    // The same review with a summary is accepted.
    db::pull_requests::reviews::add_pull_request_review(
        pool,
        pr_id,
        world.bob_id,
        "commented",
        Some("Summary"),
        Some(head.as_str()),
    )
    .await
    .unwrap();
    world.database.finish().await;
}

/// The Conversation pages comments and reviews as one run of items. A review sits where it was
/// submitted, with its inline comments, so the submit redirect, the review's notice and a reply
/// into that review all name the page that shows it.
async fn reviews_share_the_conversation_pages(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web_pull(number);
    let comments = format!("/api/repos/alice/proj/pulls/{number}/comments");
    let inline = world
        .bob
        .expect(
            "POST",
            &comments,
            json!({"body": "Inline in review", "path": "topic.txt", "line": 1, "side": "RIGHT", "commit_sha": head, "pending": true}),
            StatusCode::CREATED,
        )
        .await;
    let inline_id = inline["id"].as_i64().unwrap();
    for index in 1..=30 {
        world
            .bob
            .expect(
                "POST",
                &comments,
                json!({"body": format!("Note {index:02}")}),
                StatusCode::CREATED,
            )
            .await;
    }
    // Submitting the review makes it the thirty-first item, which opens the second page.
    let submitted = world
        .bob
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "commented"),
                ("body", "Review summary"),
                ("commit_sha", head.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert!(submitted.status.is_redirection(), "{}", submitted.text());
    let location = submitted.location.clone().unwrap();
    let review_id: i64 = location.rsplit_once("#review-").unwrap().1.parse().unwrap();
    assert_eq!(location, format!("{page}?page=2#review-{review_id}"));

    let (_, first) = world.alice.page(&page).await;
    assert!(first.contains("Note 01"));
    assert!(first.contains("?page=2"));
    assert!(!first.contains("Review summary"));
    let (_, second) = world.alice.page(&format!("{page}?page=2")).await;
    assert!(second.contains("Review summary"));
    assert!(second.contains(&format!("id=\"review-{review_id}\"")));
    assert!(second.contains(&format!("id=\"comment-{inline_id}\"")));
    assert!(!second.contains("Note 01"));

    assert_eq!(
        notification_links(&world, world.alice_id, "pull_request_review").await,
        vec![location.clone()]
    );

    // Alice's reply joins the submitted review, so Bob's notice names that review's page.
    let reply = world
        .alice
        .expect(
            "POST",
            &comments,
            json!({"body": "Answer", "in_reply_to_comment_id": inline_id}),
            StatusCode::CREATED,
        )
        .await;
    let reply_id = reply["id"].as_i64().unwrap();
    // The stored link names the comment, and following it finds the comment's page.
    let links = notification_links(&world, world.bob_id, "pull_request_comment_reply").await;
    assert_eq!(
        links,
        [format!("{page}?comment={reply_id}#comment-{reply_id}")]
    );
    let followed = world
        .bob
        .exchange(
            "GET",
            links[0].split('#').next().unwrap(),
            "text/html",
            Vec::new(),
        )
        .await;
    assert!(followed.status.is_redirection(), "{}", followed.text());
    assert_eq!(
        followed.location.unwrap(),
        format!("{page}?page=2#comment-{reply_id}")
    );
    world.database.finish().await;
}

/// The Conversation has no review limit. A review past the thousandth still shows, on the last page.
async fn reviews_past_the_old_limit_stay_on_the_conversation(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let pr_id = pull_row_id(&world, number).await;
    let pool = world.database.pool();
    for index in 0..1001 {
        let body = format!("Round {index:04}");
        db::pull_requests::reviews::add_pull_request_review(
            pool,
            pr_id,
            world.bob_id,
            "commented",
            Some(body.as_str()),
            Some(head.as_str()),
        )
        .await
        .unwrap();
    }
    let page = web_pull(number);
    let (_, first) = world.alice.page(&page).await;
    assert!(first.contains("Round 0000"));
    // 1001 items make 34 pages of 30, and the last holds the final eleven.
    let (_, last) = world.alice.page(&format!("{page}?page=34")).await;
    assert!(
        last.contains("Round 1000"),
        "the last review shows on page 34"
    );
    assert!(!last.contains("?page=35"));
    world.database.finish().await;
}

/// The pull request sort by comments counts the comments a list shows. A pending comment is hidden
/// from everyone but its author, so it must not move its pull request up the list.
async fn pull_request_sort_counts_only_shown_comments(pg: bool) {
    let mut world = World::new(pg).await;
    let (older, head) = open_pull(&world.alice, "older", world.base.as_str()).await;
    let (newer, _) = open_pull(&world.alice, "newer", world.base.as_str()).await;
    let pr_id = pull_row_id(&world, older).await;
    let pool = world.database.pool();
    db::pull_requests::reviews::add_pending_pull_request_comment(
        pool,
        pending_input(pr_id, world.bob_id, &head, "Hidden"),
    )
    .await
    .unwrap();
    let filter = db::discussions::ListFilter {
        sort: db::discussions::ListSort::MostCommented,
        ..Default::default()
    };
    let listed =
        db::pull_requests::records::list_repo_pull_requests(pool, world.repo_id, &filter, 10)
            .await
            .unwrap();
    assert_eq!(i64::from(listed[0].pull_request.number), newer);
    world.database.finish().await;
}

/// A mention that only an inline review comment makes links on the Conversation, as one in the
/// summary does.
async fn review_comment_references_link(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web_pull(number);
    world
        .bob
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/comments"),
            json!({"body": "Ask @carol", "path": "topic.txt", "line": 1, "side": "RIGHT", "commit_sha": head, "pending": true}),
            StatusCode::CREATED,
        )
        .await;
    world
        .bob
        .expect(
            "POST",
            &reviews(number),
            json!({"state": "commented", "commit_sha": head}),
            StatusCode::CREATED,
        )
        .await;
    let (_, conversation) = world.alice.page(&page).await;
    assert!(
        conversation.contains("<a href=\"/carol\" class=\"markdown-reference\">@carol</a>"),
        "{conversation}"
    );
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_pending_reply_waits_for_submit() {
    pending_reply_waits_for_submit(false).await;
}

#[tokio::test]
async fn postgres_pending_reply_waits_for_submit() {
    pending_reply_waits_for_submit(true).await;
}

#[tokio::test]
async fn sqlite_pending_review_holds_at_most_200_comments() {
    pending_review_holds_at_most_200_comments(false).await;
}

#[tokio::test]
async fn postgres_pending_review_holds_at_most_200_comments() {
    pending_review_holds_at_most_200_comments(true).await;
}

#[tokio::test]
async fn sqlite_writes_recheck_the_pull_request_and_the_pending_review() {
    writes_recheck_the_pull_request_and_the_pending_review(false).await;
}

#[tokio::test]
async fn postgres_writes_recheck_the_pull_request_and_the_pending_review() {
    writes_recheck_the_pull_request_and_the_pending_review(true).await;
}

#[tokio::test]
async fn sqlite_reviews_share_the_conversation_pages() {
    reviews_share_the_conversation_pages(false).await;
}

#[tokio::test]
async fn postgres_reviews_share_the_conversation_pages() {
    reviews_share_the_conversation_pages(true).await;
}

#[tokio::test]
async fn sqlite_reviews_past_the_old_limit_stay_on_the_conversation() {
    reviews_past_the_old_limit_stay_on_the_conversation(false).await;
}

#[tokio::test]
async fn postgres_reviews_past_the_old_limit_stay_on_the_conversation() {
    reviews_past_the_old_limit_stay_on_the_conversation(true).await;
}

#[tokio::test]
async fn sqlite_pull_request_sort_counts_only_shown_comments() {
    pull_request_sort_counts_only_shown_comments(false).await;
}

#[tokio::test]
async fn postgres_pull_request_sort_counts_only_shown_comments() {
    pull_request_sort_counts_only_shown_comments(true).await;
}

#[tokio::test]
async fn sqlite_review_comment_references_link() {
    review_comment_references_link(false).await;
}

#[tokio::test]
async fn postgres_review_comment_references_link() {
    review_comment_references_link(true).await;
}
