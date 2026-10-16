//! Assignees, milestones and list filters through the native forms and pages, on SQLite and
//! PostgreSQL. A writer manages; a reader sees; a stranger cannot be assigned; a private
//! repository answers a viewer without access as if it were missing.
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
use pulls::Client;
use serde_json::json;
use std::sync::Arc;

async fn contract(pg: bool) {
    // The native forms answer with rendered pages, which need the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let carol_id = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let proj = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "secret",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob_id,
        proj.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let carol = Client::login(&router, f.pool(), carol_id).await;
    let issue = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Bug", "body": ""}),
            StatusCode::CREATED,
        )
        .await;
    let number = issue["number"].as_i64().unwrap();
    let issue_page = format!("/alice/proj/issues/{number}");

    // Milestones: a writer creates one; a duplicate title is refused inline; a reader cannot.
    let created = alice
        .form_reply(
            "/alice/proj/milestones/new",
            &[
                ("title", "v1"),
                ("description", "First"),
                ("due_date", "2026-10-20"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
    assert_eq!(
        created.location.as_deref(),
        Some("/alice/proj/milestones/1")
    );
    let (status, page) = alice
        .form("/alice/proj/milestones/new", &[("title", "V1")])
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{page}");
    assert!(page.contains("A milestone with this title already exists."));
    let (status, _) = bob
        .form("/alice/proj/milestones/new", &[("title", "v9")])
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = alice
        .form(
            "/alice/proj/milestones/new",
            &[("title", "v8"), ("due_date", "2026-02-30")],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Assignees: a writer assigns a collaborator; a stranger cannot be assigned; a reader
    // cannot assign; a form without the session's CSRF token is refused.
    let assigned = alice
        .form_reply(&format!("{issue_page}/assignees"), &[("username", "bob")])
        .await;
    assert_eq!(assigned.status, StatusCode::SEE_OTHER);
    let (_, page) = alice.page(&issue_page).await;
    assert!(page.contains("collaboration-assignee\""), "{page}");
    assert!(page.contains("assignees/bob/remove"), "{page}");
    let (status, page) = alice
        .form(&format!("{issue_page}/assignees"), &[("username", "carol")])
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{page}");
    assert!(page.contains("Only the repository owner and collaborators can be assigned."));
    let (status, _) = bob
        .form(&format!("{issue_page}/assignees"), &[("username", "bob")])
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let forged = Client::anonymous(&router);
    let (status, _) = forged
        .form(&format!("{issue_page}/assignees"), &[("username", "bob")])
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let removed = alice
        .form_reply(&format!("{issue_page}/assignees/bob/remove"), &[])
        .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER);
    let (_, page) = alice.page(&issue_page).await;
    assert!(page.contains("No one assigned."));

    // The issue's milestone is set from its sidebar, listed on the milestone page, and cleared
    // when the milestone is deleted.
    let set = alice
        .form_reply(
            &format!("{issue_page}/milestone"),
            &[("milestone_number", "1")],
        )
        .await;
    assert_eq!(set.status, StatusCode::SEE_OTHER);
    let (_, milestone_page) = alice.page("/alice/proj/milestones/1").await;
    assert!(milestone_page.contains("Bug"), "{milestone_page}");
    let (_, list) = alice
        .page("/alice/proj/issues?milestone=1&assignee=none&sort=most-commented")
        .await;
    assert!(list.contains("Bug"), "{list}");
    // The row names its milestone on the right, and shows each assignee as an avatar.
    let assigned = alice
        .form_reply(&format!("{issue_page}/assignees"), &[("username", "bob")])
        .await;
    assert_eq!(assigned.status, StatusCode::SEE_OTHER);
    let (_, list) = alice.page("/alice/proj/issues?milestone=1").await;
    // The milestone span holds the title, with hydration markers around it.
    let milestone = list
        .split("collaboration-row-milestone\"")
        .nth(1)
        .and_then(|rest| rest.split("</span>").next())
        .unwrap_or_default();
    assert!(milestone.contains("v1"), "{list}");
    assert!(list.contains("/avatars/bob"), "{list}");
    let (status, _) = alice.page("/alice/proj/issues?assignee=nobody").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = alice.page("/alice/proj/issues?sort=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Labels: the page filters by one label name, ignoring case. The JSON list takes a comma
    // list of up to ten names in `labels`, or one name in `label` as the page does, and every
    // name must match.
    let bug = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/labels",
            json!({"name": "bug", "color": "#d73a4a"}),
            StatusCode::CREATED,
        )
        .await;
    let (status, value) = alice
        .api(
            "POST",
            &format!("/api/repos/alice/proj/issues/{number}/labels"),
            json!({"labels": [bug["id"]]}),
        )
        .await;
    assert!(status.is_success(), "{value}");
    // The add form offers the unapplied label and not the applied one.
    let docs = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/labels",
            json!({"name": "docs", "color": "#0075ca"}),
            StatusCode::CREATED,
        )
        .await;
    let (_, page) = alice.page(&issue_page).await;
    let offered = page
        .split(&format!("action=\"{issue_page}/labels\""))
        .nth(1)
        .and_then(|rest| rest.split("</form>").next())
        .unwrap_or_default();
    assert!(
        offered.contains(&format!("value=\"{}\"", docs["id"])),
        "{page}"
    );
    assert!(
        !offered.contains(&format!("value=\"{}\"", bug["id"])),
        "{page}"
    );
    let (_, list) = alice.page("/alice/proj/issues?label=BUG").await;
    assert!(list.contains("Bug"), "{list}");
    let (_, list) = alice.page("/alice/proj/issues?label=ui").await;
    assert!(list.contains("No open issues"), "{list}");
    // Several labels narrow the list to issues that carry all of them. The issue has only `bug`.
    for path in [
        "/alice/proj/issues?labels=bug,ui",
        "/alice/proj/issues?label=bug&label=ui",
    ] {
        let (status, list) = alice.page(path).await;
        assert_eq!(status, StatusCode::OK, "{list}");
        assert!(list.contains("No open issues"), "{path}: {list}");
        assert!(list.contains("Label: ui"), "{path}: {list}");
    }
    let (_, list) = alice.page("/alice/proj/issues?label=bug&label=BUG").await;
    assert!(list.contains("Bug"), "{list}");
    assert!(list.contains("Label: bug"), "{list}");
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/issues?labels=BUG",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/issues?labels=bug,ui",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert!(listed.as_array().unwrap().is_empty(), "{listed}");
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/issues?label=BUG",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/issues?label=bug&labels=ui",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert!(listed.as_array().unwrap().is_empty(), "{listed}");
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/issues?label=bug,ui",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert!(listed.as_array().unwrap().is_empty(), "{listed}");
    let (status, value) = alice
        .api(
            "GET",
            "/api/repos/alice/proj/issues?labels=a,b,c,d,e,f,g,h,i,j,k",
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert_eq!(value["error"], "validation_error", "{value}");
    // A pull request takes the same labels, milestone and assignees as an issue, and counts in
    // the milestone.
    let head = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic", &head).await;
    alice.save("topic", "topic.md", &head).await;
    let pull = alice.open_pull("Topic", "topic", "main").await;
    let pull_page = format!("/alice/proj/pulls/{}", pull["number"].as_i64().unwrap());
    let bug_id = bug["id"].to_string();
    let docs_id = docs["id"].to_string();
    let set = alice
        .form_reply(
            &format!("{pull_page}/milestone"),
            &[("milestone_number", "1")],
        )
        .await;
    assert_eq!(set.status, StatusCode::SEE_OTHER);
    let (_, page) = alice.page(&pull_page).await;
    assert!(
        page.contains(&format!("action=\"{pull_page}/milestone\"")),
        "{page}"
    );
    let (status, page) = alice
        .form(
            &format!("{pull_page}/milestone"),
            &[("milestone_number", "abc")],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{page}");
    let (_, list) = alice.page("/alice/proj/pulls?milestone=1").await;
    assert!(list.contains("Topic"), "{list}");
    assert!(list.contains("collaboration-row-milestone\""), "{list}");
    let (_, list) = alice.page("/alice/proj/pulls?milestone=none").await;
    assert!(!list.contains("Topic"), "{list}");
    let (_, page) = alice.page("/alice/proj/milestones/1").await;
    assert!(page.contains("Topic"), "{page}");
    let (_, milestones) = alice.page("/alice/proj/milestones").await;
    assert!(
        milestones.contains("0% complete. 0 closed, 2 open."),
        "{milestones}"
    );
    let set = alice
        .form_reply(
            &format!("{pull_page}/labels"),
            &[("label", bug_id.as_str())],
        )
        .await;
    assert_eq!(set.status, StatusCode::SEE_OTHER);
    let (_, page) = alice.page(&pull_page).await;
    assert!(
        page.contains(&format!("{pull_page}/labels/{bug_id}/remove")),
        "{page}"
    );
    let (_, list) = alice.page("/alice/proj/pulls?label=BUG").await;
    assert!(list.contains("Topic"), "{list}");
    let (_, list) = alice.page("/alice/proj/pulls?label=docs").await;
    assert!(!list.contains("Topic"), "{list}");
    let (status, page) = alice
        .form(&format!("{pull_page}/labels"), &[("label", "999999")])
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{page}");
    let (status, page) = bob
        .form(
            &format!("{pull_page}/labels"),
            &[("label", docs_id.as_str())],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = bob.page(&pull_page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains(&format!("{pull_page}/labels\"")), "{page}");
    let removed = alice
        .form_reply(&format!("{pull_page}/labels/{bug_id}/remove"), &[])
        .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER);
    let (_, page) = alice.page(&pull_page).await;
    assert!(
        !page.contains(&format!("{pull_page}/labels/{bug_id}/remove")),
        "{page}"
    );
    let assigned = alice
        .form_reply(&format!("{pull_page}/assignees"), &[("username", "bob")])
        .await;
    assert_eq!(assigned.status, StatusCode::SEE_OTHER);
    let (_, list) = alice.page("/alice/proj/pulls?assignee=bob").await;
    assert!(list.contains("Topic"), "{list}");

    let closed = alice
        .form_reply("/alice/proj/milestones/1/close", &[])
        .await;
    assert_eq!(closed.status, StatusCode::SEE_OTHER);
    let (_, closed_list) = alice.page("/alice/proj/milestones?state=closed").await;
    assert!(closed_list.contains("v1"), "{closed_list}");
    let deleted = alice
        .form_reply("/alice/proj/milestones/1/delete", &[])
        .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    let (_, page) = alice.page(&issue_page).await;
    assert!(page.contains("No milestone."), "{page}");
    let (_, page) = alice.page(&pull_page).await;
    assert!(page.contains("No milestone."), "{page}");

    // A private repository answers a viewer without access as missing.
    let (status, _) = carol.page("/alice/secret/milestones").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = carol.page("/alice/secret/issues/1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = carol
        .form("/alice/secret/issues/1/assignees", &[("username", "bob")])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_collaboration_sidebar_matches_forms_and_pages() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_collaboration_sidebar_matches_forms_and_pages() {
    contract(true).await;
}

/// A writer picks assignees, labels and a milestone on a new issue, and they are set in the same
/// request that creates it. A reader gets no picks, and a pick outside the lists is refused.
async fn new_issue_picks(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let carol_id = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let proj = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob_id,
        proj.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        carol_id,
        proj.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let carol = Client::login(&router, f.pool(), carol_id).await;
    let label = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/labels",
            json!({"name": "bug", "color": "d73a4a"}),
            StatusCode::CREATED,
        )
        .await;
    let label_id = label["id"].as_i64().unwrap().to_string();
    let milestone = alice
        .form_reply("/alice/proj/milestones/new", &[("title", "v1")])
        .await;
    assert_eq!(milestone.status, StatusCode::SEE_OTHER);

    // The pickers are offered to a writer only, and labels only to an administrator.
    let (status, page) = bob.page("/alice/proj/issues/new").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("name=\"assignees\""), "{page}");
    assert!(!page.contains("name=\"labels\""), "{page}");
    let (status, page) = alice.page("/alice/proj/issues/new").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("name=\"labels\""), "{page}");
    let (status, page) = carol.page("/alice/proj/issues/new").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains("name=\"labels\""), "{page}");

    // A reader cannot pick an assignee, and a label outside the list is refused. Neither creates an issue.
    let (status, page) = carol
        .form(
            "/alice/proj/issues/new",
            &[
                ("title", "Sneaky"),
                ("body", ""),
                ("intent", "create"),
                ("assignees", "alice"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{page}");
    let (status, page) = bob
        .form(
            "/alice/proj/issues/new",
            &[
                ("title", "Bad"),
                ("body", ""),
                ("intent", "create"),
                ("labels", "999"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{page}");
    assert!(
        page.contains("Choose assignees, labels and a milestone from the lists."),
        "{page}"
    );

    // The picks are set by the request that creates the issue.
    let created = alice
        .form_reply(
            "/alice/proj/issues/new",
            &[
                ("title", "Picked"),
                ("body", "Body"),
                ("intent", "create"),
                ("assignees", "alice"),
                ("labels", label_id.as_str()),
                ("milestone", "1"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
    let location = created.location.clone().unwrap();
    assert_eq!(location, "/alice/proj/issues/1");
    let (status, page) = alice.page(&location).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains(&format!("/alice/proj/issues/1/labels/{label_id}/remove")),
        "{page}"
    );
    // The sidebar's select shows the picked milestone as its selected option.
    assert!(page.contains("value=\"1\" selected"), "{page}");
    assert!(
        page.contains("/alice/proj/issues/1/assignees/alice/remove"),
        "{page}"
    );

    // A reader creates an issue without picks.
    let plain = carol
        .form_reply(
            "/alice/proj/issues/new",
            &[("title", "Plain"), ("body", ""), ("intent", "create")],
        )
        .await;
    assert_eq!(plain.status, StatusCode::SEE_OTHER);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_new_issue_applies_picks_in_its_create_request() {
    new_issue_picks(false).await;
}

#[tokio::test]
async fn postgres_new_issue_applies_picks_in_its_create_request() {
    new_issue_picks(true).await;
}
