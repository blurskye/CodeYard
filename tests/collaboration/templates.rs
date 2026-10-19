//! Issue and pull request templates: the issue chooser, the prefilled issue form, the
//! `config.yml` switch for blank issues, the pull request description template, and ticking task
//! boxes in bodies and comments. On SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::http::StatusCode;
use base64::Engine;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use database::Database;
use pulls::Client;
use serde_json::{json, Value};
use std::sync::Arc;

const BUG_TEMPLATE: &str = "---\nname: Bug report\nabout: Something is broken\ntitle: \"Bug report:\"\n---\n\n## Steps to reproduce\n";

/// `alice/proj` is public and bob can read it. Alice owns it.
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

/// Saves a file with the given text on `branch` and returns the new tip.
async fn put(client: &Client, branch: &str, path: &str, text: &str, tip: &str) -> String {
    let content = base64::engine::general_purpose::STANDARD.encode(text);
    let saved = client
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/contents/{path}"),
            json!({"content": content, "message": format!("Add {path}"), "branch": branch, "expected_revision": tip}),
            StatusCode::CREATED,
        )
        .await;
    saved["sha"].as_str().unwrap().to_owned()
}

/// The page without Dioxus node-id comments, so text can be matched across its interpolations.
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

/// The value of the first form field called `name` in the page.
fn hidden_value(html: &str, name: &str) -> String {
    let at = html
        .find(&format!("name=\"{name}\""))
        .unwrap_or_else(|| panic!("no {name} field: {html}"));
    let rest = &html[at..];
    let start = rest.find("value=\"").unwrap() + "value=\"".len();
    rest[start..].split('"').next().unwrap().to_owned()
}

async fn body_of_issue(alice: &Client, number: i64) -> String {
    alice
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/issues/{number}"),
            Value::Null,
            StatusCode::OK,
        )
        .await["body"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn chooser_and_prefill(pg: bool) {
    let (_database, _root, alice, _bob) = setup(pg).await;
    let main = put(&alice, "main", "README.md", "# proj\n", "unborn").await;
    let main = put(
        &alice,
        "main",
        ".github/ISSUE_TEMPLATE/bug.md",
        BUG_TEMPLATE,
        &main,
    )
    .await;
    let path = "/alice/proj/issues/new";

    // With templates, the form opens on the chooser, with a blank issue after it.
    let (status, page) = alice.page(path).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("Choose a template"), "{page}");
    assert!(page.contains("Bug report"), "{page}");
    assert!(page.contains("Something is broken"), "{page}");
    assert!(page.contains("Open a blank issue"), "{page}");
    assert!(!page.contains("new-title"), "{page}");

    // Picking the template opens the form prefilled from it.
    let (status, form) = alice
        .page("/alice/proj/issues/new?template=.github%2FISSUE_TEMPLATE%2Fbug.md")
        .await;
    assert_eq!(status, StatusCode::OK, "{form}");
    assert!(form.contains("value=\"Bug report:\""), "{form}");
    assert!(form.contains("## Steps to reproduce"), "{form}");
    assert!(!form.contains("Choose a template"), "{form}");

    // An empty template choice is the blank issue.
    let (_, blank) = alice.page("/alice/proj/issues/new?template=").await;
    assert!(blank.contains("new-title"), "{blank}");
    assert!(!blank.contains("Steps to reproduce"), "{blank}");

    // A name that is not a template shows the chooser again.
    let (_, unknown) = alice
        .page("/alice/proj/issues/new?template=..%2Fsecret")
        .await;
    assert!(unknown.contains("Choose a template"), "{unknown}");

    // config.yml can switch blank issues off: the chooser has no blank link and the blank form
    // is not offered.
    put(
        &alice,
        "main",
        ".github/ISSUE_TEMPLATE/config.yml",
        "blank_issues_enabled: false\n",
        &main,
    )
    .await;
    let (_, switched) = alice.page(path).await;
    assert!(switched.contains("Choose a template"), "{switched}");
    assert!(!switched.contains("Open a blank issue"), "{switched}");
    let (_, refused_blank) = alice.page("/alice/proj/issues/new?template=").await;
    assert!(
        refused_blank.contains("Choose a template"),
        "{refused_blank}"
    );
    assert!(!refused_blank.contains("new-title"), "{refused_blank}");
}

const BUG_FORM: &str = "name: Crash report\ndescription: The app crashes\nlabels: [bug]\nbody:\n  - type: input\n    attributes:\n      label: Version\n    validations:\n      required: true\n  - type: dropdown\n    attributes:\n      label: Area\n      options: [Web, Git]\n  - type: checkboxes\n    attributes:\n      label: Checks\n      options:\n        - label: I searched first\n          required: true\n";

async fn issue_forms_write_their_answers_into_the_body(pg: bool) {
    let (_database, _root, alice, _bob) = setup(pg).await;
    let main = put(&alice, "main", "README.md", "# proj\n", "unborn").await;
    put(
        &alice,
        "main",
        ".github/ISSUE_TEMPLATE/crash.yml",
        BUG_FORM,
        &main,
    )
    .await;
    let template = ".github/ISSUE_TEMPLATE/crash.yml";

    // The chooser lists the form, and opening it shows its fields instead of a description.
    let (_, chooser) = alice.page("/alice/proj/issues/new").await;
    assert!(chooser.contains("Crash report"), "{chooser}");
    let (status, page) = alice
        .page("/alice/proj/issues/new?template=.github%2FISSUE_TEMPLATE%2Fcrash.yml")
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(plain(&page).contains("Version"), "{page}");
    assert!(page.contains("name=\"form_0\""), "{page}");
    assert!(page.contains("name=\"form_2\""), "{page}");
    assert!(!page.contains("new-body"), "{page}");

    // A required box left unticked refuses the issue and keeps what was typed.
    let (status, refused) = alice
        .form(
            "/alice/proj/issues/new",
            &[
                ("template", template),
                ("title", "Crash"),
                ("form_0", "1.4"),
                ("form_1", "Git"),
                ("intent", "create"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert!(plain(&refused).contains("Checks is required."), "{refused}");
    assert!(refused.contains("value=\"1.4\""), "{refused}");

    // With the answers given, the issue body is the answers as Markdown.
    let reply = alice
        .form_reply(
            "/alice/proj/issues/new",
            &[
                ("template", template),
                ("title", "Crash"),
                ("form_0", "1.4"),
                ("form_1", "Git"),
                ("form_2", "I searched first"),
                ("intent", "create"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert_eq!(
        body_of_issue(&alice, 1).await,
        "### Version\n\n1.4\n\n### Area\n\nGit\n\n### Checks\n\n- [x] I searched first"
    );
}

async fn pull_request_template_prefills_the_description(pg: bool) {
    let (_database, _root, alice, _bob) = setup(pg).await;
    let main = put(&alice, "main", "README.md", "# proj\n", "unborn").await;
    put(
        &alice,
        "main",
        ".github/pull_request_template.md",
        "## Summary\n\nWhat changed?\n",
        &main,
    )
    .await;
    alice.branch("topic", &main).await;
    put(&alice, "topic", "feature.txt", "feature\n", &main).await;

    let (status, page) = alice
        .page("/alice/proj/pulls/new?base=main&head=topic")
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("What changed?"), "{page}");

    // A description the author already typed is kept as typed.
    let (_, typed) = alice
        .page("/alice/proj/pulls/new?base=main&head=topic&body=typed+text")
        .await;
    assert!(typed.contains("typed text"), "{typed}");
    assert!(!typed.contains("What changed?"), "{typed}");
}

/// A task box in a pull request's conversation comment ticks through the pull request's tasks
/// form. The comment keeps the rest of its text.
async fn pull_request_comment_task_ticks(pg: bool) {
    let (database, _root, alice, _bob) = setup(pg).await;
    let main = put(&alice, "main", "README.md", "# proj\n", "unborn").await;
    alice.branch("topic", &main).await;
    put(&alice, "topic", "feature.txt", "feature\n", &main).await;
    let pull = alice.open_pull("Tasks", "topic", "main").await;
    let number = pull["number"].as_i64().unwrap();
    let comment = alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/comments"),
            json!({"body": "Next:\n\n- [ ] ship\n"}),
            StatusCode::CREATED,
        )
        .await;
    let comment_id = comment["id"].as_i64().unwrap();
    let revision =
        db::pull_requests::reviews::get_pull_request_comment_by_id(database.pool(), comment_id)
            .await
            .unwrap()
            .unwrap()
            .body;
    let reply = alice
        .form_reply(
            &format!("/alice/proj/pulls/{number}/tasks"),
            &[
                ("comment", &comment_id.to_string()),
                ("task", "0"),
                ("comment_revision", &revision),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert!(reply
        .location
        .as_deref()
        .unwrap()
        .ends_with(&format!("#comment-{comment_id}")));
    let saved =
        db::pull_requests::reviews::get_pull_request_comment_by_id(database.pool(), comment_id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(saved.body, "Next:\n\n- [x] ship\n");
    let stale = alice
        .form_reply(
            &format!("/alice/proj/pulls/{number}/tasks"),
            &[
                ("comment", &comment_id.to_string()),
                ("task", "0"),
                ("comment_revision", "old"),
            ],
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    let repository = db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let refused = db::pull_requests::reviews::edit_pull_request_comment(
        database.pool(),
        saved.author_id,
        repository.id,
        saved.pull_request_id,
        comment_id,
        "overwritten",
        "not the current body",
    )
    .await
    .unwrap();
    assert!(refused.is_none());
    assert_eq!(
        db::pull_requests::reviews::get_pull_request_comment_by_id(database.pool(), comment_id)
            .await
            .unwrap()
            .unwrap()
            .body,
        saved.body
    );
}

async fn task_boxes_tick_in_the_body_and_comments(pg: bool) {
    let (database, _root, alice, bob) = setup(pg).await;
    let created = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Plan", "body": "Plan\n\n- [ ] one\n- [x] two\n"}),
            StatusCode::CREATED,
        )
        .await;
    let number = created["number"].as_i64().unwrap();
    let detail = format!("/alice/proj/issues/{number}");
    let toggle = format!("/alice/proj/issues/{number}/tasks");

    // Boxes render as buttons for the author; a reader gets none.
    let (_, page) = alice.page(&detail).await;
    assert!(page.contains("form=\"task-body\""), "{page}");
    assert!(page.contains("task-box task-box-done"), "{page}");
    let (_, reader_page) = bob.page(&detail).await;
    assert!(!reader_page.contains("form=\"task-body\""), "{reader_page}");
    assert!(reader_page.contains("disabled"), "{reader_page}");

    // A reader's tick is refused and the text stays as it was.
    let expected_updated = hidden_value(&page, "expected_updated");
    let refused = bob
        .form_reply(
            &toggle,
            &[
                ("task", "0"),
                ("expected_state", "open"),
                ("expected_updated", &expected_updated),
            ],
        )
        .await;
    assert!(!refused.status.is_redirection(), "{}", refused.text());
    assert_eq!(
        body_of_issue(&alice, number).await,
        "Plan\n\n- [ ] one\n- [x] two\n"
    );

    // The author ticks the first box. The save keeps the rest of the text.
    let expected_updated = hidden_value(&page, "expected_updated");
    let reply = alice
        .form_reply(
            &toggle,
            &[
                ("task", "0"),
                ("expected_state", "open"),
                ("expected_updated", &expected_updated),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert_eq!(
        body_of_issue(&alice, number).await,
        "Plan\n\n- [x] one\n- [x] two\n"
    );

    // A stale page is refused by the revision check.
    let stale = alice
        .form_reply(
            &toggle,
            &[
                ("task", "1"),
                ("expected_state", "open"),
                ("expected_updated", "1970-01-01T00:00:00Z"),
            ],
        )
        .await;
    assert!(!stale.status.is_redirection(), "{}", stale.text());
    assert_eq!(
        body_of_issue(&alice, number).await,
        "Plan\n\n- [x] one\n- [x] two\n"
    );

    // A comment's boxes tick through the comment's own form.
    let comment = alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/issues/{number}/comments"),
            json!({"body": "Next:\n\n- [ ] ship\n"}),
            StatusCode::CREATED,
        )
        .await;
    let comment_id = comment["id"].as_i64().unwrap().to_string();
    let stored = db::issues::get_issue_comment(database.pool(), comment_id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    let revision = stored.body.clone();
    let stale = alice
        .form_reply(
            &toggle,
            &[
                ("comment", &comment_id),
                ("task", "0"),
                ("comment_revision", "old"),
            ],
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    let reply = alice
        .form_reply(
            &toggle,
            &[
                ("comment", &comment_id),
                ("task", "0"),
                ("comment_revision", &revision),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let comments = alice
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/issues/{number}/comments"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert!(comments.to_string().contains("- [x] ship"), "{comments}");

    // The list counts the ticked boxes of the body.
    let (_, list) = alice.page("/alice/proj/issues").await;
    assert!(plain(&list).contains("2 of 2 tasks"), "{list}");
}

#[tokio::test]
async fn postgres_chooser_prefill_and_blank_switch() {
    chooser_and_prefill(true).await;
}
#[tokio::test]
async fn sqlite_chooser_prefill_and_blank_switch() {
    chooser_and_prefill(false).await;
}
#[tokio::test]
async fn postgres_pull_request_template_prefills_the_description() {
    pull_request_template_prefills_the_description(true).await;
}
#[tokio::test]
async fn sqlite_pull_request_template_prefills_the_description() {
    pull_request_template_prefills_the_description(false).await;
}
#[tokio::test]
async fn postgres_task_boxes_tick_in_the_body_and_comments() {
    task_boxes_tick_in_the_body_and_comments(true).await;
}
#[tokio::test]
async fn sqlite_task_boxes_tick_in_the_body_and_comments() {
    task_boxes_tick_in_the_body_and_comments(false).await;
}

#[tokio::test]
async fn postgres_pull_request_comment_task_ticks() {
    pull_request_comment_task_ticks(true).await;
}
#[tokio::test]
async fn sqlite_pull_request_comment_task_ticks() {
    pull_request_comment_task_ticks(false).await;
}

#[tokio::test]
async fn postgres_issue_forms_write_their_answers_into_the_body() {
    issue_forms_write_their_answers_into_the_body(true).await;
}

#[tokio::test]
async fn sqlite_issue_forms_write_their_answers_into_the_body() {
    issue_forms_write_their_answers_into_the_body(false).await;
}
