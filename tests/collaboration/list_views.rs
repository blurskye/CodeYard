//! The issue list's sort keys and filters, and the milestone list's sort, over HTTP on SQLite and
//! PostgreSQL. Each sort orders the rows, each filter narrows them, and a value the list does not
//! know is refused. The lists are read as the page model the browser loads.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db::{self, assignees::AssignedItem, discussions::DiscussionEdit, models::AccessLevel},
    repositories::lifecycle,
    storage::ObjectStore,
};
use codeyard::webui::{app::Page, pages::repos::issues};
use database::Database;
use pulls::Client;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

/// The numbers of the issues a list gives, in the order it gives them.
async fn listed(client: &Client, query: &str) -> Vec<i32> {
    let path = format!("/alice/proj/issues?{query}");
    let (status, page) = client.model(&path).await;
    assert_eq!(status, StatusCode::OK, "{path}");
    match page {
        Page::Collaboration(page) => match *page {
            issues::Page::List { items, .. } => items.iter().map(|item| item.number).collect(),
            other => panic!("{path}: not an issue list: {other:?}"),
        },
        other => panic!("{path}: not a collaboration page: {other:?}"),
    }
}

/// The titles of the milestones on the open tab, in the order the page gives them.
async fn milestone_titles(client: &Client, sort: &str) -> Vec<String> {
    let path = format!("/alice/proj/milestones?sort={sort}");
    let (status, page) = client.model(&path).await;
    assert_eq!(status, StatusCode::OK, "{path}");
    match page {
        Page::Collaboration(page) => match *page {
            issues::Page::Milestones { items, .. } => {
                items.into_iter().map(|item| item.title).collect()
            }
            other => panic!("{path}: not a milestone list: {other:?}"),
        },
        other => panic!("{path}: not a collaboration page: {other:?}"),
    }
}

/// Provisions `proj` for alice, public, with bob able to read it.
async fn provision(database: &Database, root: &std::path::Path, alice: i64, bob: i64) -> i64 {
    let repo = lifecycle::provision(
        (database.pool(), None),
        root,
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap()
    .id;
    db::repositories::access::grant_repo_access(database.pool(), bob, repo, AccessLevel::Read)
        .await
        .unwrap();
    repo
}

async fn issue_list_contract(pg: bool) {
    // The issue list is HTML, which needs the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = provision(&f, root.path(), alice_id, bob_id).await;
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let anonymous = Client::anonymous(&router);
    let pool = f.pool();

    // Four issues, one after another. Bob opens the second one, and the fourth is closed.
    let alpha = db::issues::create_issue(pool, repo, alice_id, "Alpha crash", "")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    db::issues::create_issue(pool, repo, bob_id, "Bravo slow", "")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let charlie = db::issues::create_issue(pool, repo, alice_id, "Charlie typo", "")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let delta = db::issues::create_issue(pool, repo, alice_id, "Delta closed", "")
        .await
        .unwrap();
    db::issues::update_issue(
        pool,
        delta.id,
        DiscussionEdit {
            actor_id: alice_id,
            expected_state: "open",
            expected_updated: delta.updated_at,
            title: None,
            body: None,
            state: Some("closed"),
        },
    )
    .await
    .unwrap()
    .expect("the issue closes");

    // Labels and comments: issues 1 and 3 are labelled, issue 2 has two comments and issue 3 one.
    let label = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/labels",
            json!({"name": "bug", "color": "d73a4a"}),
            StatusCode::CREATED,
        )
        .await;
    for number in [1, 3] {
        alice
            .expect(
                "POST",
                &format!("/api/repos/alice/proj/issues/{number}/labels"),
                json!({"labels": [label["id"]]}),
                StatusCode::OK,
            )
            .await;
    }
    for body in ["first", "second"] {
        alice
            .expect(
                "POST",
                "/api/repos/alice/proj/issues/2/comments",
                json!({"body": body}),
                StatusCode::CREATED,
            )
            .await;
    }
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues/3/comments",
            json!({"body": "one"}),
            StatusCode::CREATED,
        )
        .await;

    // Bob is assigned to issue 3, and issue 1 is in milestone 1.
    db::assignees::assign_user(
        pool,
        alice_id,
        repo,
        AssignedItem::Issue(charlie.id),
        bob_id,
    )
    .await
    .unwrap();
    db::milestones::create_milestone(pool, alice_id, repo, "v1", None, None)
        .await
        .unwrap();
    db::milestones::set_issue_milestone(pool, alice_id, repo, alpha.id, Some(1))
        .await
        .unwrap();

    // Renaming issue 1 last makes it the most recently changed.
    let current = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/issues/1",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/issues/1",
            json!({"expected_state": "open", "expected_updated": current["updated_at"], "state": "open", "title": "Alpha crash, renamed"}),
            StatusCode::OK,
        )
        .await;

    // Each sort key orders every issue, closed ones included.
    assert_eq!(listed(&alice, "state=all&sort=newest").await, [4, 3, 2, 1]);
    assert_eq!(listed(&alice, "state=all&sort=oldest").await, [1, 2, 3, 4]);
    assert_eq!(
        listed(&alice, "state=all&sort=most-commented").await,
        [2, 3, 4, 1]
    );
    assert_eq!(
        listed(&alice, "state=all&sort=least-commented").await,
        [4, 1, 3, 2]
    );
    assert_eq!(
        listed(&alice, "state=all&sort=recently-updated")
            .await
            .first(),
        Some(&1)
    );
    assert_eq!(
        listed(&alice, "state=all&sort=least-recently-updated")
            .await
            .last(),
        Some(&1)
    );

    // The list shows open issues unless the state says otherwise, for everyone who can read it.
    assert_eq!(listed(&alice, "").await, [3, 2, 1]);
    assert_eq!(listed(&anonymous, "").await, [3, 2, 1]);
    assert_eq!(listed(&alice, "state=closed").await, [4]);
    assert_eq!(listed(&alice, "state=all").await, [4, 3, 2, 1]);

    // Labels, assignees, milestones and authors narrow the list. `none` keeps the items with no value.
    assert_eq!(listed(&alice, "state=all&label=bug").await, [3, 1]);
    assert_eq!(listed(&alice, "assignee=bob").await, [3]);
    assert_eq!(listed(&alice, "assignee=none").await, [2, 1]);
    assert_eq!(listed(&alice, "milestone=1").await, [1]);
    assert_eq!(listed(&alice, "milestone=none").await, [3, 2]);
    assert_eq!(listed(&alice, "author=bob").await, [2]);
    assert_eq!(listed(&alice, "author=alice").await, [3, 1]);
    assert_eq!(
        listed(&alice, "state=all&assignee=none&milestone=none").await,
        [4, 2]
    );

    // A value the list does not know is refused, and so is a state or sort it does not offer.
    for query in [
        "assignee=nobody",
        "author=nobody",
        "milestone=9",
        "state=sometimes",
        "sort=sideways",
    ] {
        let (status, _) = alice.page(&format!("/alice/proj/issues?{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }

    // A milestone titled `none` is still a milestone: `no:milestone` lists the issues in none.
    let none = db::milestones::create_milestone(pool, alice_id, repo, "none", None, None)
        .await
        .unwrap();
    let bravo = db::issues::get_issue(pool, repo, 2).await.unwrap().unwrap();
    db::milestones::set_issue_milestone(pool, alice_id, repo, bravo.id, Some(none.number))
        .await
        .unwrap();
    assert_eq!(listed(&alice, "q=no:milestone").await, [3]);
    assert_eq!(listed(&alice, "q=state:all+no:milestone").await, [4, 3]);
    assert_eq!(listed(&alice, "q=milestone:none").await, [2]);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_issue_list_sorts_and_filters() {
    issue_list_contract(false).await;
}
#[tokio::test]
async fn postgres_issue_list_sorts_and_filters() {
    issue_list_contract(true).await;
}

async fn milestone_sort_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = provision(&f, root.path(), alice_id, bob_id).await;
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let pool = f.pool();

    // Milestones 1 to 4, with different due dates. Charlie has no due date and no issues.
    for (title, due) in [
        ("Alpha", Some("2026-12-15")),
        ("Bravo", Some("2026-11-01")),
        ("Charlie", None),
        ("Delta", Some("2026-10-25")),
    ] {
        db::milestones::create_milestone(pool, alice_id, repo, title, None, due)
            .await
            .unwrap();
    }
    // Alpha: one open and one closed issue, so half done. Bravo: two open, so none done. Delta:
    // three closed, so all done.
    let alpha_open = db::issues::create_issue(pool, repo, alice_id, "Alpha open", "")
        .await
        .unwrap();
    let alpha_closed = db::issues::create_issue(pool, repo, alice_id, "Alpha closed", "")
        .await
        .unwrap();
    let bravo_one = db::issues::create_issue(pool, repo, alice_id, "Bravo one", "")
        .await
        .unwrap();
    let bravo_two = db::issues::create_issue(pool, repo, alice_id, "Bravo two", "")
        .await
        .unwrap();
    let mut delta = Vec::new();
    for title in ["Delta one", "Delta two", "Delta three"] {
        delta.push(
            db::issues::create_issue(pool, repo, alice_id, title, "")
                .await
                .unwrap(),
        );
    }
    for (issue, milestone) in [
        (&alpha_open, 1),
        (&alpha_closed, 1),
        (&bravo_one, 2),
        (&bravo_two, 2),
    ]
    .into_iter()
    .chain(delta.iter().map(|issue| (issue, 4)))
    {
        db::milestones::set_issue_milestone(pool, alice_id, repo, issue.id, Some(milestone))
            .await
            .unwrap();
    }
    for issue in [&alpha_closed].into_iter().chain(delta.iter()) {
        db::issues::update_issue(
            pool,
            issue.id,
            DiscussionEdit {
                actor_id: alice_id,
                expected_state: "open",
                expected_updated: issue.updated_at,
                title: None,
                body: None,
                state: Some("closed"),
            },
        )
        .await
        .unwrap()
        .expect("the issue closes");
    }

    // Due dates order by the closest first, undated last; furthest-due reverses the dates.
    assert_eq!(
        milestone_titles(&alice, "").await,
        ["Delta", "Bravo", "Alpha", "Charlie"]
    );
    assert_eq!(
        milestone_titles(&alice, "furthest-due").await,
        ["Alpha", "Bravo", "Delta", "Charlie"]
    );
    // Progress: Delta is 100%, Alpha 50%, and Bravo and Charlie 0%.
    let complete = milestone_titles(&alice, "most-complete").await;
    assert_eq!(
        (complete[0].as_str(), complete[1].as_str()),
        ("Delta", "Alpha")
    );
    let least_complete = milestone_titles(&alice, "least-complete").await;
    assert_eq!(
        (least_complete[2].as_str(), least_complete[3].as_str()),
        ("Alpha", "Delta")
    );
    // Issue counts: Delta has three, Alpha and Bravo two each, and Charlie none.
    let most_issues = milestone_titles(&alice, "most-issues").await;
    assert_eq!(
        (most_issues[0].as_str(), most_issues[3].as_str()),
        ("Delta", "Charlie")
    );
    let least_issues = milestone_titles(&alice, "least-issues").await;
    assert_eq!(
        (least_issues[0].as_str(), least_issues[3].as_str()),
        ("Charlie", "Delta")
    );
    assert_eq!(
        milestone_titles(&alice, "alphabetical").await,
        ["Alpha", "Bravo", "Charlie", "Delta"]
    );
    assert_eq!(
        milestone_titles(&alice, "reverse-alphabetical").await,
        ["Delta", "Charlie", "Bravo", "Alpha"]
    );
    // A sort the list does not know gives the default order.
    assert_eq!(
        milestone_titles(&alice, "sideways").await,
        ["Delta", "Bravo", "Alpha", "Charlie"]
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_milestone_list_sorts() {
    milestone_sort_contract(false).await;
}
#[tokio::test]
async fn postgres_milestone_list_sorts() {
    milestone_sort_contract(true).await;
}
