//! The pull request review flow through the native forms: the review box, who may vote,
//! stale and outdated marks after a push, inline comments from the diff, conversation
//! resolution and dismissal, on SQLite and PostgreSQL.
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
use base64::Engine;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use database::Database;
use pulls::{text, Client};
use serde_json::{json, Value};
use std::sync::Arc;

/// `alice/proj`, a private repository whose `main` holds one commit. Bob and Carol have
/// write access, Dave can only read, and Erin has no access. Alice owns the repository.
struct World {
    database: Database,
    _root: tempfile::TempDir,
    router: axum::Router,
    base: String,
    alice: Client,
    bob: Client,
    carol: Client,
    dave: Client,
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
        let base = alice.save("main", "README.md", "unborn").await;
        Self {
            database,
            _root: root,
            router,
            base,
            alice,
            bob,
            carol,
            dave,
        }
    }
}

/// The web path of pull request `number`, and its REST path for JSON reads.
fn web(number: i64) -> String {
    format!("/alice/proj/pulls/{number}")
}

fn rest(number: i64, tail: &str) -> String {
    format!("/api/repos/alice/proj/pulls/{number}{tail}")
}

/// Opens a pull request from a new branch that adds one file to `base`, and returns its
/// number and head commit.
async fn open_pull(alice: &Client, branch: &str, base: &str) -> (i64, String) {
    alice.branch(branch, base).await;
    let head = alice.save(branch, &format!("{branch}.txt"), base).await;
    let pull = alice.open_pull(branch, branch, "main").await;
    (pull["number"].as_i64().unwrap(), head)
}

/// Requires one approval on `main`, with stale approvals not counting.
async fn protect(alice: &Client) {
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({
                "pattern": "main",
                "require_pull_request": true,
                "required_approving_reviews": 1,
                "dismiss_stale_reviews": true
            }),
            StatusCode::OK,
        )
        .await;
}

/// A merge attempt without its confirmation still reads the branches first, so the stored
/// head moves to what the branches hold. The attempt itself is refused.
async fn refresh_head(alice: &Client, number: i64) {
    let reply = alice
        .form_reply(&format!("{}/merge", web(number)), &[])
        .await;
    assert!(!reply.status.is_redirection(), "{}", reply.text());
}

async fn review_count(alice: &Client, number: i64) -> usize {
    alice
        .expect(
            "GET",
            &rest(number, "/reviews"),
            Value::Null,
            StatusCode::OK,
        )
        .await
        .as_array()
        .unwrap()
        .len()
}

/// The id a redirect names after `marker`, such as `#review-` or `#thread-`.
fn id_after(location: &Option<String>, marker: &str) -> i64 {
    let location = location.as_deref().expect("a redirect");
    location
        .rsplit(marker)
        .next()
        .unwrap()
        .parse()
        .unwrap_or_else(|_| panic!("{marker} in {location}"))
}

/// Whether the input that carries `value` is checked.
fn checked(html: &str, value: &str) -> bool {
    input_tag(html, value).contains("checked")
}

/// Whether the input that carries `value` is disabled.
fn disabled(html: &str, value: &str) -> bool {
    input_tag(html, value).contains("disabled")
}

/// The tag of the input that carries `value`.
fn input_tag<'a>(html: &'a str, value: &str) -> &'a str {
    let at = html
        .find(&format!("value=\"{value}\""))
        .unwrap_or_else(|| panic!("{value} radio in the page"));
    let start = html[..at].rfind('<').unwrap();
    let end = at + html[at..].find('>').unwrap();
    &html[start..=end]
}

/// Bob opens a conversation on the first line of the new file, from the diff composer.
async fn open_thread(bob: &Client, number: i64, head: &str) -> i64 {
    let reply = bob
        .form_reply(
            &format!("{}/comments", web(number)),
            &[
                ("path", "topic.txt"),
                ("side", "RIGHT"),
                ("line", "1"),
                ("commit_sha", head),
                ("from", "diff"),
                ("body", "Rename this"),
                ("intent", "comment"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert!(
        reply.location.as_deref().unwrap().ends_with(&format!(
            "/diff#thread-{}",
            id_after(&reply.location, "#thread-")
        )),
        "{:?}",
        reply.location
    );
    id_after(&reply.location, "#thread-")
}

/// The Conversation tab has one composer. A writer who is not the author sees the review choice
/// with Approve and Request changes; the author and a reader see Comment alone. The Files tab
/// lists each file with its Viewed button.
async fn one_composer_and_a_file_list_per_viewer(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, _head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web(number);

    let (status, voter) = world.bob.page(&page).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(voter.matches("id=\"review-form\"").count(), 1);
    assert!(voter.contains("name=\"state\""));
    assert!(voter.contains(&format!("formaction=\"{page}/reviews\"")));

    let (_, reader) = world.dave.page(&page).await;
    assert!(reader.contains(&format!("action=\"{page}/comments\"")));
    assert!(!reader.contains("formaction="));
    assert!(!reader.contains("id=\"review-form\""));

    let (_, author) = world.alice.page(&page).await;
    assert!(author.contains(&format!("action=\"{page}/comments\"")));
    assert!(!author.contains("value=\"approved\""));
    assert!(!author.contains("formaction="));

    let (status, files) = world.bob.page(&format!("{page}/diff")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(files.contains("aria-label=\"Changed files\""));
    assert!(files.contains("href=\"#diff-file-0\""));
    assert!(files.contains("role=\"checkbox\""));
    world.database.finish().await;
}

/// Viewed marks belong to one reader and one file at one blob. Another reader's marks are separate,
/// a reader without access gets 404, and a push that changes the file clears its mark.
async fn viewed_marks_are_per_reader_and_reset_when_the_file_changes(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let viewed = format!("{}/viewed", web(number));
    let diff = format!("{}/diff", web(number));

    let reply = world
        .bob
        .form_reply(&viewed, &[("path", "topic.txt"), ("viewed", "yes")])
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let expected = format!("{diff}#diff-file-0");
    assert_eq!(reply.location.as_deref(), Some(expected.as_str()));
    let (_, bob_files) = world.bob.page(&diff).await;
    assert!(visible_words(&bob_files).contains("1 / 1 file viewed"));
    assert!(bob_files.contains("aria-checked=\"true\""));

    // Marks belong to their reader: alice and dave still see the file unviewed.
    for reader in [&world.alice, &world.dave] {
        let (status, files) = reader.page(&diff).await;
        assert_eq!(status, StatusCode::OK);
        assert!(visible_words(&files).contains("0 / 1 file viewed"));
        assert!(!files.contains("aria-checked=\"true\""));
    }

    // A post without the CSRF token is refused, and a reader without access gets 404.
    let refused = world
        .bob
        .exchange(
            "POST",
            &viewed,
            "application/x-www-form-urlencoded",
            b"path=topic.txt&viewed=yes".to_vec(),
        )
        .await;
    assert!(refused.status.is_client_error(), "{}", refused.text());
    let erin_id = world.database.actor("erin").await;
    let erin = Client::login(&world.router, world.database.pool(), erin_id).await;
    let refused = erin
        .form_reply(&viewed, &[("path", "topic.txt"), ("viewed", "yes")])
        .await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);

    // Clearing the mark works, and marking again restores it.
    let reply = world
        .bob
        .form_reply(&viewed, &[("path", "topic.txt"), ("viewed", "no")])
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let (_, files) = world.bob.page(&diff).await;
    assert!(visible_words(&files).contains("0 / 1 file viewed"));
    world
        .bob
        .form_reply(&viewed, &[("path", "topic.txt"), ("viewed", "yes")])
        .await;

    // A push that changes the file clears its mark for every reader.
    let content = base64::engine::general_purpose::STANDARD.encode("changed\n");
    world
        .alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/contents/topic.txt",
            json!({"content": content, "message": "Change topic.txt", "branch": "topic", "expected_revision": head}),
            StatusCode::OK,
        )
        .await;
    refresh_head(&world.alice, number).await;
    let (_, files) = world.bob.page(&diff).await;
    assert!(visible_words(&files).contains("0 / 1 file viewed"));
    assert!(!files.contains("aria-checked=\"true\""));

    // Marking the new contents counts again.
    world
        .bob
        .form_reply(&viewed, &[("path", "topic.txt"), ("viewed", "yes")])
        .await;
    let (_, files) = world.bob.page(&diff).await;
    assert!(visible_words(&files).contains("1 / 1 file viewed"));
    world.database.finish().await;
}

/// A reader's merge form is refused before anything refreshes the pull request, so the stored
/// head stays put. A writer's form still refreshes it first.
async fn reader_merge_form_leaves_the_stored_head(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, first) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    // The branch moves in the store. A save through the API would refresh the pull request
    // first, so the stored head would not be stale.
    let repository =
        db::repositories::records::get_repository(world.database.pool(), "alice", "proj")
            .await
            .unwrap()
            .unwrap();
    let second = codeyard::backend::repositories::store::repo_store(&repository, None)
        .save_file_as(
            world.database.pool(),
            repository.id,
            repository.owner_user_id.unwrap(),
            &codeyard::backend::repositories::store::FileWrite {
                branch: "topic".into(),
                path: "second.txt".into(),
                message: "second".into(),
                expected_revision: first.clone(),
                creating: true,
                deleting: false,
            },
            b"second\n",
            "Refresh",
            "refresh@example.invalid",
        )
        .await
        .unwrap()
        .sha;
    assert_eq!(text(&world.alice.pull(number).await, "head_sha"), first);

    let refused = world
        .dave
        .form_reply(
            &format!("{}/merge", web(number)),
            &[
                ("expected_head", first.as_str()),
                ("expected_base", world.base.as_str()),
                ("confirm", "merge"),
            ],
        )
        .await;
    assert!(refused.status.is_client_error(), "{}", refused.text());
    let pull = world.alice.pull(number).await;
    assert_eq!(text(&pull, "head_sha"), first);
    assert_eq!(text(&pull, "base_sha"), world.base);
    assert_eq!(pull["state"], "open");

    refresh_head(&world.alice, number).await;
    assert_eq!(text(&world.alice.pull(number).await, "head_sha"), second);
    world.database.finish().await;
}

async fn native_votes_gate_the_merge(pg: bool) {
    let mut world = World::new(pg).await;
    protect(&world.alice).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web(number);

    // Carol asks for changes. The merge stays refused until she reviews again.
    let asked = world
        .carol
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "changes_requested"),
                ("body", "Needs a test"),
                ("commit_sha", head.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert!(asked.status.is_redirection(), "{}", asked.text());
    assert!(asked.location.as_deref().unwrap().contains("#review-"));
    let refused = world
        .alice
        .expect(
            "POST",
            &rest(number, "/merge"),
            json!({"expected_head": head, "expected_base": world.base}),
            StatusCode::CONFLICT,
        )
        .await;
    assert_eq!(refused["error"], "branch_protection", "{refused}");

    // Carol approves after the change, and Alice merges under the one-approval rule.
    let approved = world
        .carol
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
    assert!(approved.status.is_redirection(), "{}", approved.text());
    let merged = world.alice.merge_current(number).await;
    assert_eq!(merged["merged"], true, "{merged}");
    world.database.finish().await;
}

async fn readers_and_authors_get_no_vote(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web(number);

    let (status, reader) = world.dave.page(&page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!reader.contains("name=\"state\""));
    assert!(!reader.contains("value=\"approved\""));
    assert!(!reader.contains("value=\"changes_requested\""));
    let (_, author) = world.alice.page(&page).await;
    assert!(!author.contains("value=\"approved\""));

    // A reader may comment on the review box, but may not vote even when the form is forged.
    let comment = world
        .dave
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "commented"),
                ("body", "Reading along"),
                ("commit_sha", head.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert!(comment.status.is_redirection(), "{}", comment.text());
    let vote = world
        .dave
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
    assert!(vote.status.is_client_error(), "{}", vote.text());
    assert_eq!(review_count(&world.alice, number).await, 1);

    // The author cannot approve their own pull request.
    let own = world
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
    assert_eq!(own.status, StatusCode::BAD_REQUEST, "{}", own.text());
    assert!(own
        .text()
        .contains("Authors cannot approve their own pull request"));
    world.database.finish().await;
}

async fn stale_marks_and_refused_stale_reviews(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, first) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web(number);
    let approved = world
        .bob
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "approved"),
                ("body", ""),
                ("commit_sha", first.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert!(approved.status.is_redirection(), "{}", approved.text());
    let (_, before) = world.alice.page(&page).await;
    assert!(!before.contains("Stale"));

    // The branch moves. The review keeps its commit and is marked Stale when read.
    let second = world
        .alice
        .save("topic", "second.txt", first.as_str())
        .await;
    refresh_head(&world.alice, number).await;
    assert_eq!(text(&world.alice.pull(number).await, "head_sha"), second);
    let (_, after) = world.alice.page(&page).await;
    assert!(after.contains("Stale"));
    assert!(after.contains("Approved"));

    // A review written against the old head is refused, keeps the reviewer's draft and stores nothing.
    let count = review_count(&world.alice, number).await;
    let stale = world
        .carol
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "changes_requested"),
                ("body", "Keep this draft"),
                ("commit_sha", first.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.text());
    let html = stale.text();
    assert!(html.contains("The pull request changed after this review was started"));
    assert!(html.contains("Keep this draft"));
    assert!(checked(&html, "changes_requested"));
    assert_eq!(review_count(&world.alice, number).await, count);
    world.database.finish().await;
}

async fn inline_comments_replies_and_outdating(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, first) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web(number);
    let root = open_thread(&world.bob, number, &first).await;
    let root_text = root.to_string();

    // Alice replies in the thread from the diff; the reply joins the root.
    let reply = world
        .alice
        .form_reply(
            &format!("{page}/comments"),
            &[
                ("path", "topic.txt"),
                ("side", "RIGHT"),
                ("line", "1"),
                ("in_reply_to", root_text.as_str()),
                ("commit_sha", first.as_str()),
                ("from", "diff"),
                ("body", "Done"),
                ("intent", "comment"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let comments = world
        .alice
        .expect(
            "GET",
            &rest(number, "/comments"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let done = comments
        .as_array()
        .unwrap()
        .iter()
        .find(|comment| comment["body"] == "Done")
        .expect("the reply");
    assert_eq!(done["in_reply_to_comment_id"], root, "{done}");

    // A comment written against the base commit is refused and stores nothing.
    let before = comments.as_array().unwrap().len();
    let stale = world
        .bob
        .form_reply(
            &format!("{page}/comments"),
            &[
                ("path", "topic.txt"),
                ("side", "RIGHT"),
                ("line", "1"),
                ("commit_sha", world.base.as_str()),
                ("from", "diff"),
                ("body", "Too late"),
                ("intent", "comment"),
            ],
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.text());
    let after = world
        .alice
        .expect(
            "GET",
            &rest(number, "/comments"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(after.as_array().unwrap().len(), before);

    // After a push the conversation is outdated, and the diff still shows it under its line.
    world
        .alice
        .save("topic", "second.txt", first.as_str())
        .await;
    refresh_head(&world.alice, number).await;
    let (status, diff) = world.alice.page(&format!("{page}/diff")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(diff.contains("Outdated"));
    assert!(diff.contains(&format!("id=\"thread-{root}\"")));
    world.database.finish().await;
}

async fn resolve_and_reopen(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, first) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web(number);
    let root = open_thread(&world.bob, number, &first).await;

    // A reader cannot resolve a conversation.
    let denied = world
        .dave
        .form_reply(
            &format!("{page}/comments/{root}/resolve"),
            &[("from", "diff")],
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.text());

    // The author resolves it from the diff.
    let resolved = world
        .alice
        .form_reply(
            &format!("{page}/comments/{root}/resolve"),
            &[("from", "diff")],
        )
        .await;
    assert!(resolved.status.is_redirection(), "{}", resolved.text());
    assert!(resolved
        .location
        .as_deref()
        .unwrap()
        .ends_with(&format!("/diff#thread-{root}")));
    let (_, shown) = world.alice.page(&format!("{page}/diff")).await;
    assert!(shown.contains("Resolved"));

    // A writer reopens it from the Conversation tab.
    let reopened = world
        .carol
        .form_reply(
            &format!("{page}/comments/{root}/unresolve"),
            &[("page", "1")],
        )
        .await;
    assert!(reopened.status.is_redirection(), "{}", reopened.text());
    assert!(reopened
        .location
        .as_deref()
        .unwrap()
        .ends_with(&format!("#thread-{root}")));
    let (_, shown) = world.alice.page(&format!("{page}/diff")).await;
    assert!(!shown.contains("Resolved"));
    world.database.finish().await;
}

async fn dismissal_is_for_administrators(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web(number);
    let asked = world
        .bob
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "changes_requested"),
                ("body", "Needs work"),
                ("commit_sha", head.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert!(asked.status.is_redirection(), "{}", asked.text());
    let review = id_after(&asked.location, "#review-");

    // A writer cannot dismiss a review; the administrator can, and the reason shows.
    let refused = world
        .carol
        .form_reply(
            &format!("{page}/reviews/{review}/dismiss"),
            &[("reason", "not mine")],
        )
        .await;
    assert!(refused.status.is_client_error(), "{}", refused.text());
    let dismissed = world
        .alice
        .form_reply(
            &format!("{page}/reviews/{review}/dismiss"),
            &[("reason", "Superseded")],
        )
        .await;
    assert!(dismissed.status.is_redirection(), "{}", dismissed.text());
    let reviews = world
        .alice
        .expect(
            "GET",
            &rest(number, "/reviews"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let state = reviews
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == review)
        .expect("the review")["state"]
        .clone();
    assert_eq!(state, "dismissed");
    let (_, shown) = world.alice.page(&page).await;
    assert!(shown.contains("Dismissed: Superseded"));
    world.database.finish().await;
}

async fn forms_need_their_csrf_token(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let body = format!("state=approved&body=&commit_sha={head}&intent=submit");
    let refused = world
        .bob
        .exchange(
            "POST",
            &format!("{}/reviews", web(number)),
            "application/x-www-form-urlencoded",
            body.into_bytes(),
        )
        .await;
    assert!(refused.status.is_client_error(), "{}", refused.text());
    assert_eq!(review_count(&world.alice, number).await, 0);
    world.database.finish().await;
}

/// The visible words of a page around its Viewed count, for a failure message.
fn visible_words(html: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let at = text.find("viewed").unwrap_or(0);
    text.get(at.saturating_sub(300)..)
        .unwrap_or_default()
        .chars()
        .take(600)
        .collect()
}

/// The text of the merge panel's blocker list, or an empty string when the page has none.
fn blockers_of(html: &str) -> &str {
    let Some(at) = html.find("collaboration-blockers") else {
        return "";
    };
    let end = html[at..].find("</ul>").map_or(html.len(), |end| at + end);
    &html[at..end]
}

/// The merge panel names what blocks a merge for a writer, and each item leaves the list once
/// it is met. Readers and visitors get no list, and the merge refuses with the first item.
async fn page_lists_what_blocks_the_merge(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web(number);
    let root = open_thread(&world.bob, number, &head).await;
    world
        .alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({
                "pattern": "main",
                "require_pull_request": true,
                "required_approving_reviews": 1,
                "dismiss_stale_reviews": true,
                "require_status_checks": true,
                "required_status_checks": ["external/check"],
                "require_conversation_resolution": true
            }),
            StatusCode::OK,
        )
        .await;
    let status = |state: &str| json!({"context": "external/check", "state": state});
    let statuses = format!("/api/repos/alice/proj/commits/{head}/statuses");
    world
        .alice
        .expect("POST", &statuses, status("failure"), StatusCode::CREATED)
        .await;

    let (_, shown) = world.alice.page(&page).await;
    let list = blockers_of(&shown);
    assert!(
        list.contains("1 approving review required, found 0"),
        "{list}"
    );
    assert!(
        list.contains("Required status checks not passing: external/check (failure)"),
        "{list}"
    );
    assert!(
        list.contains("1 conversation(s) must be resolved before merging"),
        "{list}"
    );
    let refused = world
        .alice
        .expect(
            "POST",
            &rest(number, "/merge"),
            json!({"expected_head": head, "expected_base": world.base}),
            StatusCode::CONFLICT,
        )
        .await;
    assert_eq!(refused["error"], "branch_protection", "{refused}");
    let refusal = format!("Pull request #{number}: 1 approving review required, found 0");
    assert_eq!(text(&refused, "message"), refusal);
    assert!(list.contains(&refusal), "{list}");
    let (_, read) = world.dave.page(&page).await;
    assert!(blockers_of(&read).is_empty(), "{}", blockers_of(&read));
    let (_, visited) = Client::anonymous(&world.router).page(&page).await;
    assert!(blockers_of(&visited).is_empty());

    // Each item leaves the list when it is met: the status first, then the approval, then the
    // conversation.
    world
        .alice
        .expect("POST", &statuses, status("success"), StatusCode::CREATED)
        .await;
    let (_, shown) = world.alice.page(&page).await;
    let list = blockers_of(&shown);
    assert!(!list.contains("external/check"), "{list}");
    assert!(list.contains("1 approving review required"), "{list}");
    let approved = world
        .carol
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
    assert!(approved.status.is_redirection(), "{}", approved.text());
    let (_, shown) = world.alice.page(&page).await;
    let list = blockers_of(&shown);
    assert!(!list.contains("approving review"), "{list}");
    assert!(
        list.contains("1 conversation(s) must be resolved before merging"),
        "{list}"
    );
    let resolved = world
        .alice
        .form_reply(
            &format!("{page}/comments/{root}/resolve"),
            &[("from", "diff")],
        )
        .await;
    assert!(resolved.status.is_redirection(), "{}", resolved.text());
    let (_, shown) = world.alice.page(&page).await;
    assert!(blockers_of(&shown).is_empty(), "{}", blockers_of(&shown));
    let merged = world.alice.merge_current(number).await;
    assert_eq!(merged["merged"], true, "{merged}");
    world.database.finish().await;
}

/// A draft pull request lists the same text the merge refuses it with.
async fn page_says_a_draft_cannot_merge(pg: bool) {
    let mut world = World::new(pg).await;
    world.alice.branch("draft-topic", world.base.as_str()).await;
    let head = world
        .alice
        .save("draft-topic", "draft-topic.txt", world.base.as_str())
        .await;
    let pull = world
        .alice
        .open_draft("draft-topic", "draft-topic", "main")
        .await;
    let number = pull["number"].as_i64().unwrap();
    let refused = world
        .alice
        .expect(
            "POST",
            &rest(number, "/merge"),
            json!({"expected_head": head, "expected_base": world.base}),
            StatusCode::CONFLICT,
        )
        .await;
    assert_eq!(refused["error"], "unsupported_merge", "{refused}");
    assert!(
        text(&refused, "message").contains("is a draft"),
        "{refused}"
    );
    let (_, shown) = world.alice.page(&web(number)).await;
    assert!(
        blockers_of(&shown).contains(text(&refused, "message")),
        "{}",
        blockers_of(&shown)
    );
    world.database.finish().await;
}

/// The list follows the method the form preselects. Merge commits come first, so an out-of-date
/// head is listed for them. With merge commits disabled, squash is preselected: the same head is
/// not listed, and the squash merge goes through.
async fn page_follows_the_preselected_merge_method(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, _head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web(number);
    // main moves on, so the topic branch is out of date with it.
    world
        .alice
        .save("main", "moved.txt", world.base.as_str())
        .await;
    world
        .alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({"pattern": "main", "require_pull_request": true, "require_linear_history": true}),
            StatusCode::OK,
        )
        .await;
    refresh_head(&world.alice, number).await;

    // A merge commit is refused on a linear branch, so the form starts on squash: the merge option
    // is disabled, and the list holds squash's blockers, which the linear rule does not stop.
    let (_, shown) = world.alice.page(&page).await;
    assert!(disabled(&shown, "merge"), "{shown}");
    assert!(checked(&shown, "squash"), "{shown}");
    assert!(
        !blockers_of(&shown).contains("Merge commits are not allowed"),
        "{}",
        blockers_of(&shown)
    );
    assert!(
        !blockers_of(&shown).contains("out of date"),
        "{}",
        blockers_of(&shown)
    );

    world
        .alice
        .expect(
            "PATCH",
            "/api/repos/alice/proj",
            json!({"is_private": true, "description": "Merge methods", "allow_merge_commit": false}),
            StatusCode::OK,
        )
        .await;
    let (_, shown) = world.alice.page(&page).await;
    assert!(
        !blockers_of(&shown).contains("out of date"),
        "{}",
        blockers_of(&shown)
    );
    let pull = world.alice.pull(number).await;
    let merged = world
        .alice
        .expect(
            "POST",
            &rest(number, "/merge"),
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"], "merge_method": "squash"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(merged["merged"], true, "{merged}");
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_viewed_marks_are_per_reader_and_reset_when_the_file_changes() {
    viewed_marks_are_per_reader_and_reset_when_the_file_changes(false).await;
}

#[tokio::test]
async fn postgres_viewed_marks_are_per_reader_and_reset_when_the_file_changes() {
    viewed_marks_are_per_reader_and_reset_when_the_file_changes(true).await;
}

#[tokio::test]
async fn sqlite_one_composer_and_a_file_list_per_viewer() {
    one_composer_and_a_file_list_per_viewer(false).await;
}

#[tokio::test]
async fn postgres_one_composer_and_a_file_list_per_viewer() {
    one_composer_and_a_file_list_per_viewer(true).await;
}

#[tokio::test]
async fn sqlite_reader_merge_form_leaves_the_stored_head() {
    reader_merge_form_leaves_the_stored_head(false).await;
}

#[tokio::test]
async fn postgres_reader_merge_form_leaves_the_stored_head() {
    reader_merge_form_leaves_the_stored_head(true).await;
}

#[tokio::test]
async fn sqlite_native_votes_gate_the_merge() {
    native_votes_gate_the_merge(false).await;
}

#[tokio::test]
async fn postgres_native_votes_gate_the_merge() {
    native_votes_gate_the_merge(true).await;
}

#[tokio::test]
async fn sqlite_page_lists_what_blocks_the_merge() {
    page_lists_what_blocks_the_merge(false).await;
}

#[tokio::test]
async fn postgres_page_lists_what_blocks_the_merge() {
    page_lists_what_blocks_the_merge(true).await;
}

#[tokio::test]
async fn sqlite_page_says_a_draft_cannot_merge() {
    page_says_a_draft_cannot_merge(false).await;
}

#[tokio::test]
async fn postgres_page_says_a_draft_cannot_merge() {
    page_says_a_draft_cannot_merge(true).await;
}

#[tokio::test]
async fn sqlite_page_follows_the_preselected_merge_method() {
    page_follows_the_preselected_merge_method(false).await;
}

#[tokio::test]
async fn postgres_page_follows_the_preselected_merge_method() {
    page_follows_the_preselected_merge_method(true).await;
}

#[tokio::test]
async fn sqlite_readers_and_authors_get_no_vote() {
    readers_and_authors_get_no_vote(false).await;
}

#[tokio::test]
async fn postgres_readers_and_authors_get_no_vote() {
    readers_and_authors_get_no_vote(true).await;
}

#[tokio::test]
async fn sqlite_stale_marks_and_refused_stale_reviews() {
    stale_marks_and_refused_stale_reviews(false).await;
}

#[tokio::test]
async fn postgres_stale_marks_and_refused_stale_reviews() {
    stale_marks_and_refused_stale_reviews(true).await;
}

#[tokio::test]
async fn sqlite_inline_comments_replies_and_outdating() {
    inline_comments_replies_and_outdating(false).await;
}

#[tokio::test]
async fn postgres_inline_comments_replies_and_outdating() {
    inline_comments_replies_and_outdating(true).await;
}

#[tokio::test]
async fn sqlite_resolve_and_reopen() {
    resolve_and_reopen(false).await;
}

#[tokio::test]
async fn postgres_resolve_and_reopen() {
    resolve_and_reopen(true).await;
}

#[tokio::test]
async fn sqlite_dismissal_is_for_administrators() {
    dismissal_is_for_administrators(false).await;
}

#[tokio::test]
async fn postgres_dismissal_is_for_administrators() {
    dismissal_is_for_administrators(true).await;
}

#[tokio::test]
async fn sqlite_forms_need_their_csrf_token() {
    forms_need_their_csrf_token(false).await;
}

#[tokio::test]
async fn postgres_forms_need_their_csrf_token() {
    forms_need_their_csrf_token(true).await;
}

#[tokio::test]
async fn sqlite_reviewer_requests_refuse_visitors_readers_and_forgery() {
    reviewer_requests_refuse_visitors_readers_and_forgery(false).await;
}

#[tokio::test]
async fn postgres_reviewer_requests_refuse_visitors_readers_and_forgery() {
    reviewer_requests_refuse_visitors_readers_and_forgery(true).await;
}

#[tokio::test]
async fn sqlite_reviewer_requests_are_asked_again_and_removed() {
    reviewer_requests_are_asked_again_and_removed(false).await;
}

#[tokio::test]
async fn postgres_reviewer_requests_are_asked_again_and_removed() {
    reviewer_requests_are_asked_again_and_removed(true).await;
}

/// The reviewers REST lists as pending requests.
async fn requested_reviewers(alice: &Client, number: i64) -> Vec<Value> {
    alice
        .expect(
            "GET",
            &rest(number, "/requested_reviewers"),
            Value::Null,
            StatusCode::OK,
        )
        .await
        .as_array()
        .unwrap()
        .clone()
}

/// The review-request notifications a user holds, as (title, link) pairs.
async fn review_requests_for(pool: &db::DbPool, user_id: i64) -> Vec<(String, Option<String>)> {
    db::notifications::records::list_notifications_for_user(pool, user_id, 100, false)
        .await
        .unwrap()
        .into_iter()
        .filter(|notification| notification.kind == "pull_request_review_requested")
        .map(|notification| (notification.title, notification.link))
        .collect()
}

/// The Reviewers panel posts to the same rules as the requested_reviewers routes. A visitor, a
/// reader, a forged form, a stranger and an unknown name all change nothing.
async fn reviewer_requests_refuse_visitors_readers_and_forgery(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, _head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web(number);
    let requests = format!("{page}/requested_reviewers");

    let visitor = Client::anonymous(&world.router);
    let anonymous = visitor
        .form_reply(&requests, &[("username", "carol")])
        .await;
    assert!(anonymous.status.is_client_error(), "{}", anonymous.text());

    let reader = world
        .dave
        .form_reply(&requests, &[("username", "carol")])
        .await;
    assert_eq!(reader.status, StatusCode::NOT_FOUND, "{}", reader.text());

    let forged = world
        .bob
        .exchange(
            "POST",
            &requests,
            "application/x-www-form-urlencoded",
            b"username=carol".to_vec(),
        )
        .await;
    assert!(forged.status.is_client_error(), "{}", forged.text());

    // A name that is not an account keeps the typed name in the form.
    let unknown = world
        .bob
        .form_reply(&requests, &[("username", "nobody-here")])
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{}", unknown.text());
    let html = unknown.text();
    assert!(html.contains("value=\"nobody-here\""), "{html}");
    assert!(html.contains("The requested resource was not found or you do not have access."));

    // The author cannot be asked, and a reader cannot be asked either.
    let author = world
        .bob
        .form_reply(&requests, &[("username", "alice")])
        .await;
    assert_eq!(author.status, StatusCode::BAD_REQUEST, "{}", author.text());
    assert!(author
        .text()
        .contains("Cannot request review from pull request author"));
    let stranger = world
        .bob
        .form_reply(&requests, &[("username", "dave")])
        .await;
    assert_eq!(
        stranger.status,
        StatusCode::FORBIDDEN,
        "{}",
        stranger.text()
    );

    assert!(requested_reviewers(&world.alice, number).await.is_empty());
    world.database.finish().await;
}

/// A writer asks a reviewer, the reviewer decides, and the writer asks again. The reviewer is
/// notified as the REST route notifies. Removal is for writers, and a closed pull request takes
/// no new requests.
async fn reviewer_requests_are_asked_again_and_removed(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", world.base.as_str()).await;
    let page = web(number);
    let requests = format!("{page}/requested_reviewers");

    let asked = world
        .bob
        .form_reply(&requests, &[("username", "carol")])
        .await;
    assert!(asked.status.is_redirection(), "{}", asked.text());
    assert_eq!(
        asked.location.as_deref(),
        Some(format!("{page}#reviewers").as_str())
    );
    let remove = format!("{requests}/carol/remove");
    let pending = requested_reviewers(&world.alice, number).await;
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0]["username"], "carol");
    let carol_id = pending[0]["user_id"].as_i64().unwrap();
    let (_, shown) = world.bob.page(&page).await;
    assert!(shown.contains("Review requested"), "{shown}");
    assert!(shown.contains(&remove), "{shown}");
    let notices = review_requests_for(world.database.pool(), carol_id).await;
    let link = format!("/alice/proj/pulls/{number}");
    let title = format!("bob requested your review on pull request #{number}");
    assert_eq!(notices, vec![(title, Some(link))]);

    // Carol decides. Her request is consumed, and the panel offers to ask her again.
    let decided = world
        .carol
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "changes_requested"),
                ("body", "Needs a test"),
                ("commit_sha", head.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert!(decided.status.is_redirection(), "{}", decided.text());
    assert!(requested_reviewers(&world.alice, number).await.is_empty());
    let (_, shown) = world.bob.page(&page).await;
    assert!(shown.contains("Changes requested"), "{shown}");
    assert!(shown.contains("Re-request review"), "{shown}");

    // Asking again makes one new request. Asking while it is open changes nothing.
    let again = world
        .bob
        .form_reply(&requests, &[("username", "carol")])
        .await;
    assert!(again.status.is_redirection(), "{}", again.text());
    let repeat = world
        .bob
        .form_reply(&requests, &[("username", "carol")])
        .await;
    assert!(repeat.status.is_redirection(), "{}", repeat.text());
    assert_eq!(requested_reviewers(&world.alice, number).await.len(), 1);
    let notices = review_requests_for(world.database.pool(), carol_id).await;
    assert_eq!(notices.len(), 2);
    let (_, shown) = world.bob.page(&page).await;
    assert!(shown.contains("Review requested"), "{shown}");
    assert!(!shown.contains("Re-request review"), "{shown}");

    // A reader cannot take the request back. A writer can, and a second attempt finds nothing.
    let reader = world.dave.form_reply(&remove, &[]).await;
    assert_eq!(reader.status, StatusCode::NOT_FOUND, "{}", reader.text());
    assert_eq!(requested_reviewers(&world.alice, number).await.len(), 1);
    let removed = world.bob.form_reply(&remove, &[]).await;
    assert!(removed.status.is_redirection(), "{}", removed.text());
    assert_eq!(
        removed.location.as_deref(),
        Some(format!("{page}#reviewers").as_str())
    );
    assert!(requested_reviewers(&world.alice, number).await.is_empty());
    let gone = world.bob.form_reply(&remove, &[]).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "{}", gone.text());

    // Once the pull request is closed, no one can ask for a review.
    let updated = text(&world.alice.pull(number).await, "updated_at").to_owned();
    let closed = world
        .alice
        .form_reply(
            &format!("{page}/state"),
            &[
                ("expected_state", "open"),
                ("expected_updated", updated.as_str()),
                ("state", "closed"),
            ],
        )
        .await;
    assert!(closed.status.is_redirection(), "{}", closed.text());
    let refused = world
        .bob
        .form_reply(&requests, &[("username", "carol")])
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.text());
    assert!(requested_reviewers(&world.alice, number).await.is_empty());
    world.database.finish().await;
}

/// The value of the input named `name` on a page.
fn hidden_value(html: &str, name: &str) -> String {
    let at = html
        .find(&format!("name=\"{name}\""))
        .unwrap_or_else(|| panic!("{name} in the page"));
    let start = html[..at].rfind('<').unwrap();
    let end = at + html[at..].find('>').unwrap();
    let tag = &html[start..=end];
    let value = tag.find("value=\"").unwrap() + "value=\"".len();
    tag[value..].split('"').next().unwrap().to_owned()
}

/// A preview of a draft written against an earlier head keeps that head, so the later submit is
/// still checked against it and refused as stale.
async fn preview_keeps_the_commit_a_draft_was_written_against(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, first) = open_pull(&world.alice, "topic", &world.base).await;
    let page = web(number);
    let second = world.alice.save("topic", "second.txt", &first).await;
    refresh_head(&world.alice, number).await;
    assert_eq!(text(&world.alice.pull(number).await, "head_sha"), second);

    let preview = world
        .bob
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "approved"),
                ("body", "Keep this"),
                ("commit_sha", first.as_str()),
                ("intent", "preview"),
            ],
        )
        .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.text());
    assert_eq!(hidden_value(&preview.text(), "commit_sha"), first);

    let submit = world
        .bob
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "approved"),
                ("body", "Keep this"),
                ("commit_sha", first.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert_eq!(submit.status, StatusCode::CONFLICT, "{}", submit.text());
    let html = submit.text();
    assert!(html.contains("The pull request changed after this review was started"));
    assert!(html.contains("Keep this"));
    assert_eq!(hidden_value(&html, "commit_sha"), first);
    assert_eq!(review_count(&world.alice, number).await, 0);
    world.database.finish().await;
}

/// A new conversation must sit on a line of the pull request's diff, on the side it names. A
/// reply to an outdated thread still goes in.
async fn new_conversations_must_sit_on_the_diff(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, first) = open_pull(&world.alice, "topic", &world.base).await;
    let page = web(number);
    let comments = rest(number, "/comments");

    // A file the pull request does not change, a line past the end of the new file, and the left
    // side of a file that has no old lines are all refused, through REST first.
    for (path, side, line, message) in [
        (
            "missing.txt",
            "RIGHT",
            1,
            "path must be a file this pull request changes",
        ),
        (
            "topic.txt",
            "RIGHT",
            9,
            "line must be a line of the pull request diff on that side",
        ),
        (
            "topic.txt",
            "LEFT",
            1,
            "line must be a line of the pull request diff on that side",
        ),
    ] {
        let (status, refused) = world
            .bob
            .api(
                "POST",
                &comments,
                json!({"body": "Check", "path": path, "line": line, "side": side, "commit_sha": first}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
        assert_eq!(refused["error"], "validation_error", "{refused}");
        assert_eq!(refused["message"], message, "{refused}");
    }
    assert_eq!(review_count(&world.alice, number).await, 0);

    // The same refusal from the composer keeps the reviewer's text on the diff.
    let refused = world
        .bob
        .form_reply(
            &format!("{page}/comments"),
            &[
                ("path", "topic.txt"),
                ("side", "LEFT"),
                ("line", "1"),
                ("commit_sha", first.as_str()),
                ("from", "diff"),
                ("body", "Left side words"),
                ("intent", "comment"),
            ],
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{}",
        refused.text()
    );
    let html = refused.text();
    assert!(html.contains("line must be a line of the pull request diff on that side"));
    assert!(html.contains("Left side words"));

    // A line of the diff still opens a conversation, and after a push a reply to it still goes in.
    let root = open_thread(&world.bob, number, &first).await;
    let root_text = root.to_string();
    let second = world.alice.save("topic", "second.txt", &first).await;
    refresh_head(&world.alice, number).await;
    let reply = world
        .alice
        .form_reply(
            &format!("{page}/comments"),
            &[
                ("path", "topic.txt"),
                ("side", "RIGHT"),
                ("line", "1"),
                ("in_reply_to", root_text.as_str()),
                ("commit_sha", second.as_str()),
                ("from", "diff"),
                ("body", "Still here"),
                ("intent", "comment"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    world.database.finish().await;
}

/// Once a pull request is closed, a refused review keeps its text on the page, read only, and
/// offers no form to submit it again.
async fn closed_pull_request_keeps_the_refused_text_read_only(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, head) = open_pull(&world.alice, "topic", &world.base).await;
    let page = web(number);
    let updated = text(&world.alice.pull(number).await, "updated_at").to_owned();
    let closed = world
        .alice
        .form_reply(
            &format!("{page}/state"),
            &[
                ("expected_state", "open"),
                ("expected_updated", updated.as_str()),
                ("state", "closed"),
            ],
        )
        .await;
    assert!(closed.status.is_redirection(), "{}", closed.text());

    let refused = world
        .bob
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "approved"),
                ("body", "Keep this closed text"),
                ("commit_sha", head.as_str()),
                ("intent", "submit"),
            ],
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{}",
        refused.text()
    );
    let html = refused.text();
    assert!(html.contains("This pull request is closed; your text was not saved."));
    assert!(!html.contains("id=\"review-form\""));
    // The served page is the production render, so the text must sit in the textarea as plain
    // escaped content, with no hydration comment markers inside the field.
    let field = textarea_holding(&html, "Keep this closed text");
    assert!(!field.contains("<!--"), "{field}");
    world.database.finish().await;
}

/// The textarea that holds `text`, markup included.
fn textarea_holding<'a>(html: &'a str, text: &str) -> &'a str {
    let at = html
        .find(text)
        .unwrap_or_else(|| panic!("{text} in the page"));
    let start = html[..at].rfind("<textarea").unwrap();
    let end = at + html[at..].find("</textarea>").unwrap();
    &html[start..end]
}

#[tokio::test]
async fn sqlite_preview_keeps_the_commit_a_draft_was_written_against() {
    preview_keeps_the_commit_a_draft_was_written_against(false).await;
}

#[tokio::test]
async fn postgres_preview_keeps_the_commit_a_draft_was_written_against() {
    preview_keeps_the_commit_a_draft_was_written_against(true).await;
}

#[tokio::test]
async fn sqlite_new_conversations_must_sit_on_the_diff() {
    new_conversations_must_sit_on_the_diff(false).await;
}

#[tokio::test]
async fn postgres_new_conversations_must_sit_on_the_diff() {
    new_conversations_must_sit_on_the_diff(true).await;
}

#[tokio::test]
async fn sqlite_closed_pull_request_keeps_the_refused_text_read_only() {
    closed_pull_request_keeps_the_refused_text_read_only(false).await;
}

#[tokio::test]
async fn postgres_closed_pull_request_keeps_the_refused_text_read_only() {
    closed_pull_request_keeps_the_refused_text_read_only(true).await;
}

/// Saves a file with `content` on `branch` through the editor API.
async fn save_text(alice: &Client, branch: &str, file: &str, content: &str, tip: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(content);
    alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/contents/{file}"),
            json!({"content": encoded, "message": format!("Add {file}"), "branch": branch, "expected_revision": tip}),
            StatusCode::CREATED,
        )
        .await;
}

/// A file whose patch is too large to display blocks comments on that file only. A comment on a
/// small file of the same pull request still goes in, and one on the large file gets the size
/// error rather than an unavailable service.
async fn oversized_patch_blocks_only_its_own_comments(pg: bool) {
    let mut world = World::new(pg).await;
    world.alice.branch("mixed", &world.base).await;
    let small = world.alice.save("mixed", "small.txt", &world.base).await;
    // About 325 KB of text: one file's patch may hold 256 KiB, so this file alone exceeds it.
    let large = "line of text\n".repeat(25_000);
    save_text(&world.alice, "mixed", "huge.txt", &large, &small).await;
    let pull = world.alice.open_pull("Mixed", "mixed", "main").await;
    let number = pull["number"].as_i64().unwrap();
    let comments = rest(number, "/comments");

    let (status, small_comment) = world
        .bob
        .api(
            "POST",
            &comments,
            json!({"body": "Small file", "path": "small.txt", "line": 1, "side": "RIGHT"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{small_comment}");

    let (status, refused) = world
        .bob
        .api(
            "POST",
            &comments,
            json!({"body": "Large file", "path": "huge.txt", "line": 1, "side": "RIGHT"}),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{refused}");
    assert_eq!(refused["error"], "response_too_large", "{refused}");
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_oversized_patch_blocks_only_its_own_comments() {
    oversized_patch_blocks_only_its_own_comments(false).await;
}

#[tokio::test]
async fn postgres_oversized_patch_blocks_only_its_own_comments() {
    oversized_patch_blocks_only_its_own_comments(true).await;
}

/// Commits `files` (top-level names with their text) as the whole tree on top of `parent_sha`,
/// and points `branch` at it. The commit is written straight to the repository.
async fn commit_tree(
    world: &World,
    branch: &str,
    parent_sha: &str,
    files: &[(&str, &str)],
) -> String {
    let repository =
        db::repositories::records::get_repository(world.database.pool(), "alice", "proj")
            .await
            .unwrap()
            .unwrap();
    let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let parent = git
        .find_commit(git2::Oid::from_str(parent_sha).unwrap())
        .unwrap();
    let mut tree = git.treebuilder(None).unwrap();
    for &(name, text) in files {
        let blob = git.blob(text.as_bytes()).unwrap();
        tree.insert(name, blob, 0o100644).unwrap();
    }
    let tree = git.find_tree(tree.write().unwrap()).unwrap();
    let signature = git2::Signature::now("alice", "alice@example.test").unwrap();
    let commit = git
        .commit(None, &signature, &signature, "Commit", &tree, &[&parent])
        .unwrap();
    git.reference(&format!("refs/heads/{branch}"), commit, true, "test commit")
        .unwrap();
    commit.to_string()
}

/// A renamed and edited file keeps its rename in the comparison. A left line on the old side and
/// a right line inside the hunk the diff displays are accepted; a line outside those hunks is
/// refused. Another file of the same pull request still takes a comment.
async fn renamed_file_anchors_follow_the_displayed_patch(pg: bool) {
    let mut world = World::new(pg).await;
    let old: String = (1..=60).map(|line| format!("line {line}\n")).collect();
    // One line of 60 changes, so the rename is detected at high similarity.
    let edited = old.replacen("line 5\n", "changed 5\n", 1);
    let base = commit_tree(
        &world,
        "base2",
        &world.base,
        &[("README.md", "README.md\n"), ("old.txt", old.as_str())],
    )
    .await;
    let head = commit_tree(
        &world,
        "renamed",
        &base,
        &[
            ("README.md", "README.md\n"),
            ("new.txt", edited.as_str()),
            ("plain.txt", "plain\n"),
        ],
    )
    .await;
    let pull = world.alice.open_pull("Renamed", "renamed", "base2").await;
    let number = pull["number"].as_i64().unwrap();
    let comments = rest(number, "/comments");
    assert_eq!(text(&pull, "head_sha"), head);

    for (side, line, expected) in [
        ("LEFT", 3, StatusCode::CREATED),
        ("RIGHT", 3, StatusCode::CREATED),
        ("LEFT", 50, StatusCode::BAD_REQUEST),
        ("RIGHT", 50, StatusCode::BAD_REQUEST),
    ] {
        let (status, reply) = world
            .bob
            .api(
                "POST",
                &comments,
                json!({"body": "Rename anchor", "path": "new.txt", "line": line, "side": side}),
            )
            .await;
        assert_eq!(status, expected, "{side} {line}: {reply}");
    }
    let (status, plain) = world
        .bob
        .api(
            "POST",
            &comments,
            json!({"body": "Other file", "path": "plain.txt", "line": 1, "side": "RIGHT"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{plain}");
    world.database.finish().await;
}

/// File names are matched literally: a name with glob characters or a leading colon is found as
/// itself, and a name only a pattern would match is not a file of the pull request.
async fn literal_file_names_are_read_literally(pg: bool) {
    let mut world = World::new(pg).await;
    commit_tree(
        &world,
        "literal",
        &world.base,
        &[
            ("README.md", "README.md\n"),
            ("a[1].txt", "one\n"),
            (":weird.txt", "two\n"),
        ],
    )
    .await;
    let pull = world.alice.open_pull("Literal", "literal", "main").await;
    let number = pull["number"].as_i64().unwrap();
    let comments = rest(number, "/comments");
    for path in ["a[1].txt", ":weird.txt"] {
        let (status, reply) = world
            .bob
            .api(
                "POST",
                &comments,
                json!({"body": "Literal name", "path": path, "line": 1, "side": "RIGHT"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{path}: {reply}");
    }
    let (status, refused) = world
        .bob
        .api(
            "POST",
            &comments,
            json!({"body": "Pattern only", "path": "a1.txt", "line": 1, "side": "RIGHT"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(
        refused["message"], "path must be a file this pull request changes",
        "{refused}"
    );
    world.database.finish().await;
}

/// A new conversation written against an earlier head is refused once the head has moved, through
/// REST and through the form, and nothing is stored.
async fn stale_head_refuses_new_conversations(pg: bool) {
    let mut world = World::new(pg).await;
    let (number, first) = open_pull(&world.alice, "topic", &world.base).await;
    let page = web(number);
    let comments = rest(number, "/comments");
    let before = world
        .alice
        .expect("GET", &comments, Value::Null, StatusCode::OK)
        .await
        .as_array()
        .unwrap()
        .len();
    let second = world.alice.save("topic", "second.txt", &first).await;
    refresh_head(&world.alice, number).await;
    assert_eq!(text(&world.alice.pull(number).await, "head_sha"), second);

    let (status, refused) = world
        .bob
        .api(
            "POST",
            &comments,
            json!({"body": "Old head", "path": "topic.txt", "line": 1, "side": "RIGHT", "commit_sha": first}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "revision_conflict", "{refused}");

    let refused = world
        .bob
        .form_reply(
            &format!("{page}/comments"),
            &[
                ("path", "topic.txt"),
                ("side", "RIGHT"),
                ("line", "1"),
                ("commit_sha", first.as_str()),
                ("from", "diff"),
                ("body", "Stale draft"),
                ("intent", "comment"),
            ],
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.text());
    assert!(refused.text().contains("Stale draft"));

    let after = world
        .alice
        .expect("GET", &comments, Value::Null, StatusCode::OK)
        .await
        .as_array()
        .unwrap()
        .len();
    assert_eq!(after, before);
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_renamed_file_anchors_follow_the_displayed_patch() {
    renamed_file_anchors_follow_the_displayed_patch(false).await;
}

#[tokio::test]
async fn postgres_renamed_file_anchors_follow_the_displayed_patch() {
    renamed_file_anchors_follow_the_displayed_patch(true).await;
}

#[tokio::test]
async fn sqlite_literal_file_names_are_read_literally() {
    literal_file_names_are_read_literally(false).await;
}

#[tokio::test]
async fn postgres_literal_file_names_are_read_literally() {
    literal_file_names_are_read_literally(true).await;
}

#[tokio::test]
async fn sqlite_stale_head_refuses_new_conversations() {
    stale_head_refuses_new_conversations(false).await;
}

#[tokio::test]
async fn postgres_stale_head_refuses_new_conversations() {
    stale_head_refuses_new_conversations(true).await;
}
