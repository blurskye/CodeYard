//! Reactions on issue descriptions and comments, on pull request reviews and their inline comments,
//! and the timeline of changes, through the native forms. A reaction toggles, counts every user
//! once, and shows as pressed only for the user who gave it. Pending comments take none. On SQLite
//! and PostgreSQL.
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

/// `alice/proj` is public and bob can read it. Alice owns it.
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

async fn setup(pg: bool) -> (Database, tempfile::TempDir, Client, Client) {
    bundle::install();
    let database = Database::new(pg).await;
    database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = database.actor("alice").await;
    let bob_id = database.actor("bob").await;
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
    (database, root, alice, bob)
}

async fn reactions_toggle_and_count(pg: bool) {
    let (mut database, _root, alice, bob) = setup(pg).await;
    for title in ["First", "Second"] {
        alice
            .expect(
                "POST",
                "/api/repos/alice/proj/issues",
                json!({"title": title, "body": "Body"}),
                StatusCode::CREATED,
            )
            .await;
    }
    let comment = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues/1/comments",
            json!({"body": "A comment"}),
            StatusCode::CREATED,
        )
        .await["id"]
        .as_i64()
        .unwrap()
        .to_string();
    let other = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues/2/comments",
            json!({"body": "Elsewhere"}),
            StatusCode::CREATED,
        )
        .await["id"]
        .as_i64()
        .unwrap()
        .to_string();

    // Both users react to the description; only bob reacts to the comment.
    for client in [&alice, &bob] {
        let reply = client
            .form_reply("/alice/proj/issues/1/react", &[("content", "heart")])
            .await;
        assert!(reply.status.is_redirection(), "{}", reply.text());
    }
    let reply = bob
        .form_reply(
            "/alice/proj/issues/1/react",
            &[("content", "rocket"), ("comment", &comment)],
        )
        .await;
    assert_eq!(
        reply.location.as_deref(),
        Some(format!("/alice/proj/issues/1#comment-{comment}").as_str())
    );
    let (_, page) = bob.page("/alice/proj/issues/1").await;
    let page = plain(&page);
    assert!(page.contains("aria-pressed=\"true\""));
    let (_, page) = alice.page("/alice/proj/issues/1").await;
    let page = plain(&page);
    // Alice's heart is pressed and counted twice; the rocket is bob's and counted once.
    assert!(page.contains("aria-pressed=\"true\""));
    assert!(page.contains("aria-pressed=\"false\""));
    assert!(page.contains("collaboration-reaction-count\">2<"));
    assert!(page.contains("collaboration-reaction-count\">1<"));

    // The same reaction again takes it back.
    let reply = bob
        .form_reply("/alice/proj/issues/1/react", &[("content", "heart")])
        .await;
    assert!(reply.status.is_redirection());
    let (_, page) = alice.page("/alice/proj/issues/1").await;
    let page = plain(&page);
    assert!(!page.contains("collaboration-reaction-count\">2<"));

    // Unknown reactions are refused, and a comment from another issue is not found.
    let (status, _) = bob
        .form("/alice/proj/issues/1/react", &[("content", "skull")])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = bob
        .form(
            "/alice/proj/issues/1/react",
            &[("content", "heart"), ("comment", &other)],
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    database.finish().await;
}

#[tokio::test]
async fn postgres_reactions_toggle_and_count() {
    reactions_toggle_and_count(true).await;
}
#[tokio::test]
async fn sqlite_reactions_toggle_and_count() {
    reactions_toggle_and_count(false).await;
}

/// The id a redirect names after `marker`, such as `#review-`.
fn id_after(location: Option<&str>, marker: &str) -> i64 {
    let location = location.expect("a redirect");
    location
        .rsplit_once(marker)
        .unwrap_or_else(|| panic!("{marker} in {location}"))
        .1
        .parse()
        .unwrap()
}

/// The reaction bar of the form that names `field` (`comment` or `review`) with `id`.
fn bar<'a>(page: &'a str, field: &str, id: i64) -> &'a str {
    let needle = format!("name=\"{field}\" value=\"{id}\"");
    let at = page
        .find(needle.as_str())
        .unwrap_or_else(|| panic!("{needle} is not in the page"));
    let start = page[..at].rfind("<form").expect("the bar's form");
    let end = at
        + page[at..]
            .find("</form>")
            .expect("the end of the bar's form");
    &page[start..end]
}

/// Reactions on a review and on the inline comments in it, and on a reply in a diff thread. A
/// comment of a pending review is invisible to everyone else and takes no reaction at all.
async fn reactions_on_reviews_and_diff_comments(pg: bool) {
    let (mut database, _root, alice, bob) = setup(pg).await;
    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic", &base).await;
    let head = alice.save("topic", "topic.txt", &base).await;
    let number = alice.open_pull("topic", "topic", "main").await["number"]
        .as_i64()
        .unwrap();
    let page = format!("/alice/proj/pulls/{number}");

    // Bob starts a review with an inline comment, and submits it with a summary.
    let start = bob
        .form_reply(
            &format!("{page}/comments"),
            &[
                ("path", "topic.txt"),
                ("side", "RIGHT"),
                ("line", "1"),
                ("commit_sha", head.as_str()),
                ("from", "diff"),
                ("body", "Rename this"),
                ("intent", "review"),
            ],
        )
        .await;
    assert!(start.status.is_redirection(), "{}", start.text());
    let root = id_after(start.location.as_deref(), "#thread-");
    let submit = bob
        .form_reply(
            &format!("{page}/reviews"),
            &[
                ("state", "commented"),
                ("body", "Looks fine"),
                ("commit_sha", head.as_str()),
            ],
        )
        .await;
    assert!(submit.status.is_redirection(), "{}", submit.text());
    let review = id_after(submit.location.as_deref(), "#review-");

    // Alice replies in the thread from the diff.
    let reply = alice
        .form_reply(
            &format!("{page}/comments"),
            &[
                ("path", "topic.txt"),
                ("side", "RIGHT"),
                ("line", "1"),
                ("in_reply_to", root.to_string().as_str()),
                ("commit_sha", head.as_str()),
                ("from", "diff"),
                ("body", "Done"),
                ("intent", "comment"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let comments = alice
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/pulls/{number}/comments"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let reply_id = comments
        .as_array()
        .unwrap()
        .iter()
        .find(|comment| comment["body"] == "Done")
        .expect("the reply")["id"]
        .as_i64()
        .unwrap();

    // Bob's next comment waits in a pending review. Nobody else sees it, and nobody reacts to it.
    let hidden = bob
        .form_reply(
            &format!("{page}/comments"),
            &[
                ("path", "topic.txt"),
                ("side", "RIGHT"),
                ("line", "1"),
                ("commit_sha", head.as_str()),
                ("from", "diff"),
                ("body", "Still private"),
                ("intent", "review"),
            ],
        )
        .await;
    assert!(hidden.status.is_redirection(), "{}", hidden.text());
    let hidden_id = id_after(hidden.location.as_deref(), "#thread-");

    // Alice likes the review; Bob likes it too. From the diff, a reaction returns to the diff.
    let liked = alice
        .form_reply(
            &format!("{page}/react"),
            &[
                ("content", "heart"),
                ("review", review.to_string().as_str()),
            ],
        )
        .await;
    assert_eq!(
        liked.location.as_deref(),
        Some(format!("{page}#review-{review}").as_str())
    );
    let rocket = bob
        .form_reply(
            &format!("{page}/react"),
            &[
                ("content", "rocket"),
                ("review", review.to_string().as_str()),
            ],
        )
        .await;
    assert!(rocket.status.is_redirection(), "{}", rocket.text());
    let from_diff = alice
        .form_reply(
            &format!("{page}/react?from=diff"),
            &[("content", "heart"), ("comment", root.to_string().as_str())],
        )
        .await;
    assert_eq!(
        from_diff.location.as_deref(),
        Some(format!("{page}/diff#comment-{root}").as_str())
    );
    let split = bob
        .form_reply(
            &format!("{page}/react?from=diff&view=split"),
            &[
                ("content", "rocket"),
                ("comment", reply_id.to_string().as_str()),
            ],
        )
        .await;
    assert_eq!(
        split.location.as_deref(),
        Some(format!("{page}/diff?view=split#comment-{reply_id}").as_str())
    );

    // Nobody can react to the pending comment, not even its author. A visitor cannot react at all.
    for client in [&alice, &bob] {
        let (status, _) = client
            .form(
                &format!("{page}/react?from=diff"),
                &[
                    ("content", "eyes"),
                    ("comment", hidden_id.to_string().as_str()),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let visitor = Client::anonymous(&alice.router);
    let (status, _) = visitor
        .form(
            &format!("{page}/react"),
            &[
                ("content", "heart"),
                ("review", review.to_string().as_str()),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The conversation shows the review and its inline comment with their counts, and the
    // diff shows the thread with its reply. Alice's pressed reactions are hers alone.
    let (_, conversation) = alice.page(&page).await;
    let conversation = plain(&conversation);
    let review_bar = bar(&conversation, "review", review);
    assert!(review_bar.contains("aria-pressed=\"true\""), "{review_bar}");
    assert!(
        review_bar.contains("collaboration-reaction-count\">1<"),
        "{review_bar}"
    );
    assert!(bar(&conversation, "comment", root).contains("aria-pressed=\"true\""));
    assert!(bar(&conversation, "comment", reply_id).contains("collaboration-reaction-count\">1<"));
    assert!(!conversation.contains("Still private"));
    assert!(!conversation.contains(&format!("name=\"comment\" value=\"{hidden_id}\"")));
    let (_, diff) = alice.page(&format!("{page}/diff")).await;
    let diff = plain(&diff);
    assert!(bar(&diff, "comment", root).contains("aria-pressed=\"true\""));
    assert!(bar(&diff, "comment", reply_id).contains("aria-pressed=\"false\""));
    assert!(!diff.contains("Still private"));
    assert!(!diff.contains(&format!("thread-{hidden_id}")));
    // Bob sees his own pending comment, without a bar, and his rocket on the reply.
    let (_, own) = bob.page(&format!("{page}/diff")).await;
    let own = plain(&own);
    assert!(own.contains("Still private"));
    assert!(!own.contains(&format!("name=\"comment\" value=\"{hidden_id}\"")));
    assert!(bar(&own, "comment", reply_id).contains("aria-pressed=\"true\""));

    // The same reaction again takes it back, so only Alice's heart is left on the review.
    let again = bob
        .form_reply(
            &format!("{page}/react"),
            &[
                ("content", "rocket"),
                ("review", review.to_string().as_str()),
            ],
        )
        .await;
    assert!(again.status.is_redirection(), "{}", again.text());
    let (_, conversation) = alice.page(&page).await;
    let conversation = plain(&conversation);
    assert_eq!(
        bar(&conversation, "review", review)
            .matches("collaboration-reaction-count")
            .count(),
        1
    );

    // An archived repository refuses reactions.
    database
        .execute("UPDATE repositories SET is_archived = TRUE", &[])
        .await;
    let (status, _) = alice
        .form(
            &format!("{page}/react?from=diff"),
            &[("content", "eyes"), ("comment", root.to_string().as_str())],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    database.finish().await;
}

#[tokio::test]
async fn postgres_reactions_on_reviews_and_diff_comments() {
    reactions_on_reviews_and_diff_comments(true).await;
}
#[tokio::test]
async fn sqlite_reactions_on_reviews_and_diff_comments() {
    reactions_on_reviews_and_diff_comments(false).await;
}

/// Closing, renaming, labelling and assigning an issue leave a line in its discussion.
async fn timeline_shows_changes(pg: bool) {
    let (mut database, _root, alice, _bob) = setup(pg).await;
    let issue = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Before", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/issues/1",
            json!({"expected_state": "open", "expected_updated": issue["updated_at"], "title": "After", "state": "closed"}),
            StatusCode::OK,
        )
        .await;
    let label = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/labels",
            json!({"name": "bug", "color": "d73a4a"}),
            StatusCode::CREATED,
        )
        .await;
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues/1/labels",
            json!({"labels": [label["id"]]}),
            StatusCode::OK,
        )
        .await;
    let reply = alice
        .form_reply("/alice/proj/issues/1/assignees", &[("username", "alice")])
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let (_, page) = alice.page("/alice/proj/issues/1").await;
    let page = plain(&page);
    for expected in [
        "changed the title",
        "closed this",
        "added the ",
        "assigned ",
    ] {
        assert!(page.contains(expected), "{expected}");
    }
    // The last mention is the label chip itself; earlier ones are in the inlined stylesheet.
    let chip = &page[page.rfind("collaboration-event-chip").expect("label chip")..];
    assert!(
        chip[..chip.find("</span>").unwrap()].ends_with(">bug"),
        "{chip}"
    );
    database.finish().await;
}

/// Each discussion page loads the changes in its own span, with its own limit. Changes after the
/// first page's limit still show on the page they fall in.
async fn timeline_events_load_for_each_page(pg: bool) {
    let (mut database, _root, alice, _bob) = setup(pg).await;
    let issue = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Changes", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    let issue_id = issue["id"].as_i64().unwrap();
    let repo = db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let actor = db::users::accounts::get_user_by_username(database.pool(), "alice")
        .await
        .unwrap()
        .unwrap();
    // 210 changes come before the first page's comments, more than one page's limit of 200.
    for index in 0..210 {
        db::timeline::record_event(
            database.pool(),
            repo.id,
            "issue",
            issue_id,
            actor.id,
            "labeled",
            &format!("early-{index}"),
            "",
        )
        .await
        .unwrap();
    }
    // Thirty-one comments make two pages; the second begins at the thirty-first.
    for index in 1..=31 {
        alice
            .expect(
                "POST",
                "/api/repos/alice/proj/issues/1/comments",
                json!({"body": format!("Note {index:02}")}),
                StatusCode::CREATED,
            )
            .await;
    }
    for index in 0..10 {
        db::timeline::record_event(
            database.pool(),
            repo.id,
            "issue",
            issue_id,
            actor.id,
            "labeled",
            &format!("late-{index}"),
            "",
        )
        .await
        .unwrap();
    }
    let (_, first) = alice.page("/alice/proj/issues/1").await;
    assert!(
        first.contains("early-0"),
        "the first page loads its changes"
    );
    let (_, second) = alice.page("/alice/proj/issues/1?page=2").await;
    for index in 0..10 {
        assert!(
            second.contains(&format!("late-{index}")),
            "page 2 shows its change late-{index}"
        );
    }
    database.finish().await;
}

#[tokio::test]
async fn postgres_timeline_shows_changes() {
    timeline_shows_changes(true).await;
}
#[tokio::test]
async fn sqlite_timeline_shows_changes() {
    timeline_shows_changes(false).await;
}

#[tokio::test]
async fn sqlite_timeline_events_load_for_each_page() {
    timeline_events_load_for_each_page(false).await;
}

#[tokio::test]
async fn postgres_timeline_events_load_for_each_page() {
    timeline_events_load_for_each_page(true).await;
}

/// A removal that a reader attempts, and one that removes nothing, leave no line in the timeline.
async fn timeline_records_only_real_unassignments(pg: bool) {
    let (mut database, _root, alice, bob) = setup(pg).await;
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Assigned", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    let reply = alice
        .form_reply("/alice/proj/issues/1/assignees", &[("username", "alice")])
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let pool = database.pool();
    let issue = db::issues::get_issue(pool, proj_id(pool).await, 1)
        .await
        .unwrap()
        .unwrap()
        .id;

    // Bob reads the issue but cannot change its assignees, so his removal is refused.
    let (status, _) = bob
        .form("/alice/proj/issues/1/assignees/alice/remove", &[])
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(unassigned_lines(pool, issue).await, 0);
    // A name that is no user removes nothing.
    let reply = alice
        .form_reply("/alice/proj/issues/1/assignees/nobody/remove", &[])
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert_eq!(unassigned_lines(pool, issue).await, 0);
    // The first removal leaves one line; removing the same name again leaves none.
    for _ in 0..2 {
        let reply = alice
            .form_reply("/alice/proj/issues/1/assignees/alice/remove", &[])
            .await;
        assert!(reply.status.is_redirection(), "{}", reply.text());
    }
    assert_eq!(unassigned_lines(pool, issue).await, 1);
    database.finish().await;
}

#[tokio::test]
async fn postgres_timeline_records_only_real_unassignments() {
    timeline_records_only_real_unassignments(true).await;
}
#[tokio::test]
async fn sqlite_timeline_records_only_real_unassignments() {
    timeline_records_only_real_unassignments(false).await;
}

/// A reaction sent from a later discussion page comes back to that page. One sent from the first
/// page keeps the plain address.
async fn reaction_returns_to_its_discussion_page(pg: bool) {
    let (mut database, _root, alice, _bob) = setup(pg).await;
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Long", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    // A discussion page holds thirty comments, so the thirty-first is on the second page.
    let mut comments = Vec::new();
    for index in 0..31 {
        let comment = alice
            .expect(
                "POST",
                "/api/repos/alice/proj/issues/1/comments",
                json!({"body": format!("Comment {index}")}),
                StatusCode::CREATED,
            )
            .await;
        comments.push(comment["id"].as_i64().unwrap().to_string());
    }
    let (_, second) = alice.page("/alice/proj/issues/1?page=2").await;
    assert!(
        plain(&second).contains("action=\"/alice/proj/issues/1/react?page=2\""),
        "the reaction bars on page 2 name it"
    );
    let last = &comments[30];
    let reply = alice
        .form_reply(
            "/alice/proj/issues/1/react?page=2",
            &[("content", "eyes"), ("comment", last)],
        )
        .await;
    assert_eq!(
        reply.location.as_deref(),
        Some(format!("/alice/proj/issues/1?page=2#comment-{last}").as_str())
    );
    let first = &comments[0];
    let reply = alice
        .form_reply(
            "/alice/proj/issues/1/react",
            &[("content", "eyes"), ("comment", first)],
        )
        .await;
    assert_eq!(
        reply.location.as_deref(),
        Some(format!("/alice/proj/issues/1#comment-{first}").as_str())
    );
    database.finish().await;
}

#[tokio::test]
async fn postgres_reaction_returns_to_its_discussion_page() {
    reaction_returns_to_its_discussion_page(true).await;
}
#[tokio::test]
async fn sqlite_reaction_returns_to_its_discussion_page() {
    reaction_returns_to_its_discussion_page(false).await;
}

/// The id of alice's repository, for checks that read the database.
async fn proj_id(pool: &db::DbPool) -> i64 {
    db::repositories::records::get_repository(pool, "alice", "proj")
        .await
        .unwrap()
        .unwrap()
        .id
}

/// The number of `unassigned` lines in the timeline of an issue.
async fn unassigned_lines(pool: &db::DbPool, issue: i64) -> usize {
    db::timeline::list_events(pool, "issue", issue, None, None, db::timeline::EVENT_LIMIT)
        .await
        .unwrap()
        .iter()
        .filter(|event| event.kind == "unassigned")
        .count()
}

/// The reactions of `viewer` on an issue, summed over every reaction.
async fn reaction_total(pool: &db::DbPool, issue: i64, viewer: i64) -> i64 {
    db::reactions::reaction_counts(pool, "issue", &[issue], viewer)
        .await
        .unwrap()
        .iter()
        .map(|row| row.count)
        .sum()
}

/// Asserts that a toggle was refused for the reactor's access.
fn assert_denied(result: anyhow::Result<bool>) {
    let error = result.expect_err("the toggle is refused");
    assert!(
        matches!(
            error.downcast_ref::<db::discussions::DiscussionWriteRefusal>(),
            Some(db::discussions::DiscussionWriteRefusal::Denied)
        ),
        "{error:#}"
    );
}

/// A toggle checks the reactor's access in its own transaction, not only when the page was read.
/// A repository that turns private, a grant that is taken back and an archived repository each
/// refuse the toggle and leave the reactions as they were.
async fn toggles_check_access_in_their_transaction(pg: bool) {
    let (mut database, _root, _alice, _bob) = setup(pg).await;
    let pool = database.pool();
    let owner = db::users::accounts::get_user_by_username(pool, "alice")
        .await
        .unwrap()
        .unwrap()
        .id;
    let bob = db::users::accounts::get_user_by_username(pool, "bob")
        .await
        .unwrap()
        .unwrap()
        .id;
    let repo = proj_id(pool).await;
    let issue = db::issues::create_issue(pool, repo, owner, "Reacted", "")
        .await
        .unwrap()
        .id;
    let toggle = |actor: i64| {
        db::reactions::toggle_reaction(
            pool,
            actor,
            repo,
            issue,
            db::reactions::Subject::Issue,
            issue,
            "heart",
        )
    };

    // Bob reads the repository through the public grant, and cannot toggle once it is private.
    database
        .execute("UPDATE repositories SET is_private = TRUE", &[])
        .await;
    assert_denied(toggle(bob).await);
    assert_eq!(reaction_total(pool, issue, owner).await, 0);

    // With a grant he can react. Taking the grant back refuses his next toggle and keeps his heart.
    db::repositories::access::grant_repo_access(pool, bob, repo, db::models::AccessLevel::Read)
        .await
        .unwrap();
    assert!(toggle(bob).await.unwrap());
    db::repositories::access::revoke_repo_access(pool, bob, repo)
        .await
        .unwrap();
    assert_denied(toggle(bob).await);
    assert_eq!(reaction_total(pool, issue, owner).await, 1);

    // An archived repository refuses the owner too.
    database
        .execute("UPDATE repositories SET is_archived = TRUE", &[])
        .await;
    assert_denied(toggle(owner).await);
    assert_eq!(reaction_total(pool, issue, owner).await, 1);
    database.finish().await;
}

#[tokio::test]
async fn postgres_toggles_check_access_in_their_transaction() {
    toggles_check_access_in_their_transaction(true).await;
}
#[tokio::test]
async fn sqlite_toggles_check_access_in_their_transaction() {
    toggles_check_access_in_their_transaction(false).await;
}

/// Two toggles of one reaction at the same time are two changes: one sets it and the other takes it
/// back. Both run, so the reaction ends unset and the two toggles report different results.
async fn concurrent_toggles_do_not_lose_updates(pg: bool) {
    let (mut database, _root, _alice, _bob) = setup(pg).await;
    let pool = database.pool();
    let owner = db::users::accounts::get_user_by_username(pool, "alice")
        .await
        .unwrap()
        .unwrap()
        .id;
    let repo = proj_id(pool).await;
    let issue = db::issues::create_issue(pool, repo, owner, "Contended", "")
        .await
        .unwrap()
        .id;
    for _ in 0..10 {
        let (first, second) = tokio::join!(
            db::reactions::toggle_reaction(
                pool,
                owner,
                repo,
                issue,
                db::reactions::Subject::Issue,
                issue,
                "eyes"
            ),
            db::reactions::toggle_reaction(
                pool,
                owner,
                repo,
                issue,
                db::reactions::Subject::Issue,
                issue,
                "eyes"
            ),
        );
        assert_ne!(
            first.unwrap(),
            second.unwrap(),
            "both toggles set the reaction"
        );
        assert_eq!(reaction_total(pool, issue, owner).await, 0);
    }
    database.finish().await;
}

#[tokio::test]
async fn postgres_concurrent_toggles_do_not_lose_updates() {
    concurrent_toggles_do_not_lose_updates(true).await;
}
#[tokio::test]
async fn sqlite_concurrent_toggles_do_not_lose_updates() {
    concurrent_toggles_do_not_lose_updates(false).await;
}
