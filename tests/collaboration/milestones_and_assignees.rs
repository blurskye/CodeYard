//! Assignees, milestones and the issue list filters on disposable SQLite and PostgreSQL
//! databases. Setup failure fails the test; nothing is skipped or mocked.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;

use codeyard::backend::{
    db::{
        self,
        assignees::{AssignedItem, AssignmentRefusal},
        discussions::{DiscussionEdit, ListFilter, ListSort, Pick},
        milestones::MilestoneWriteRefusal,
        models::AccessLevel,
    },
    repositories::lifecycle,
};
use database::Database;

fn assignment_refusal(error: anyhow::Error) -> AssignmentRefusal {
    *error
        .downcast_ref::<AssignmentRefusal>()
        .expect("an assignment refusal")
}

fn milestone_refusal(error: anyhow::Error) -> MilestoneWriteRefusal {
    *error
        .downcast_ref::<MilestoneWriteRefusal>()
        .expect("a milestone refusal")
}

/// The numbers a filter lists, in the order the list gives them.
async fn issue_numbers(pool: &db::DbPool, repo: i64, filter: ListFilter<'_>) -> Vec<i32> {
    db::issues::list_repo_issues(pool, repo, &filter, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.issue.number)
        .collect()
}

/// The numbers a filter lists, sorted, for a set comparison.
async fn issue_set(pool: &db::DbPool, repo: i64, filter: ListFilter<'_>) -> Vec<i32> {
    let mut numbers = issue_numbers(pool, repo, filter).await;
    numbers.sort_unstable();
    numbers
}

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("alice").await;
    let reader = f.actor("bob").await;
    let writer = f.actor("dave").await;
    let stranger = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let pool = f.pool();
    let repo = lifecycle::provision(
        (pool, None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap()
    .id;
    db::repositories::access::grant_repo_access(pool, reader, repo, AccessLevel::Read)
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(pool, writer, repo, AccessLevel::Write)
        .await
        .unwrap();

    let first = db::issues::create_issue(pool, repo, owner, "First", "")
        .await
        .unwrap();
    let second = db::issues::create_issue(pool, repo, owner, "Second", "")
        .await
        .unwrap();
    let on_first = AssignedItem::Issue(first.id);

    // Writers and the owner assign. Readers cannot, and only collaborators can be assigned.
    db::assignees::assign_user(pool, owner, repo, on_first, reader)
        .await
        .unwrap();
    db::assignees::assign_user(pool, owner, repo, on_first, writer)
        .await
        .unwrap();
    db::assignees::assign_user(pool, owner, repo, on_first, writer)
        .await
        .unwrap();
    assert_eq!(
        db::assignees::list_assignees(pool, on_first).await.unwrap(),
        vec!["bob", "dave"]
    );
    assert_eq!(
        assignment_refusal(
            db::assignees::assign_user(pool, reader, repo, on_first, owner)
                .await
                .unwrap_err()
        ),
        AssignmentRefusal::Denied
    );
    assert_eq!(
        assignment_refusal(
            db::assignees::assign_user(pool, owner, repo, on_first, stranger)
                .await
                .unwrap_err()
        ),
        AssignmentRefusal::Unassignable
    );
    assert_eq!(
        assignment_refusal(
            db::assignees::assign_user(pool, owner, repo, AssignedItem::Issue(i64::MAX), reader)
                .await
                .unwrap_err()
        ),
        AssignmentRefusal::NotFound
    );
    // At most ten assignees per issue.
    for index in 0..8 {
        let member = f.actor(&format!("member{index}")).await;
        db::repositories::access::grant_repo_access(pool, member, repo, AccessLevel::Read)
            .await
            .unwrap();
        db::assignees::assign_user(pool, owner, repo, on_first, member)
            .await
            .unwrap();
    }
    let extra = f.actor("member8").await;
    db::repositories::access::grant_repo_access(pool, extra, repo, AccessLevel::Read)
        .await
        .unwrap();
    assert_eq!(
        assignment_refusal(
            db::assignees::assign_user(pool, owner, repo, on_first, extra)
                .await
                .unwrap_err()
        ),
        AssignmentRefusal::TooMany
    );
    // A reader is refused for any name, and a name that is no user removes nothing.
    assert_eq!(
        assignment_refusal(
            db::assignees::unassign_user(pool, reader, repo, on_first, "nobody")
                .await
                .unwrap_err()
        ),
        AssignmentRefusal::Denied
    );
    assert!(
        !db::assignees::unassign_user(pool, owner, repo, on_first, "nobody")
            .await
            .unwrap()
    );
    assert!(
        db::assignees::unassign_user(pool, owner, repo, on_first, "bob")
            .await
            .unwrap()
    );
    assert!(
        !db::assignees::unassign_user(pool, owner, repo, on_first, "bob")
            .await
            .unwrap()
    );
    // The batch lookup gives each listed issue its own assignees, and never reads pull request rows.
    db::assignees::assign_user(pool, owner, repo, AssignedItem::Issue(second.id), writer)
        .await
        .unwrap();
    let batch = db::assignees::list_item_assignees(pool, false, &[first.id, second.id])
        .await
        .unwrap();
    assert_eq!(
        batch.iter().filter(|(id, _)| *id == first.id).count(),
        9,
        "{batch:?}"
    );
    assert!(batch.contains(&(second.id, "dave".to_owned())), "{batch:?}");
    assert!(db::assignees::list_item_assignees(pool, true, &[first.id])
        .await
        .unwrap()
        .is_empty());
    assert!(db::assignees::list_item_assignees(pool, false, &[])
        .await
        .unwrap()
        .is_empty());
    assert!(db::assignees::unassign_user(
        pool,
        owner,
        repo,
        AssignedItem::Issue(second.id),
        "dave"
    )
    .await
    .unwrap());
    let assignable = db::assignees::list_assignable_usernames(pool, repo)
        .await
        .unwrap();
    assert!(assignable.contains(&"alice".to_owned()));
    assert!(!assignable.contains(&"carol".to_owned()));

    // Milestones: numbers per repository, titles unique without regard to case.
    let v1 = db::milestones::create_milestone(
        pool,
        owner,
        repo,
        "v1",
        Some("first"),
        Some("2026-10-20"),
    )
    .await
    .unwrap();
    assert_eq!((v1.number, v1.due_date.as_deref()), (1, Some("2026-10-20")));
    assert_eq!(
        milestone_refusal(
            db::milestones::create_milestone(pool, owner, repo, "V1", None, None)
                .await
                .unwrap_err()
        ),
        MilestoneWriteRefusal::DuplicateTitle
    );
    assert_eq!(
        milestone_refusal(
            db::milestones::create_milestone(pool, reader, repo, "v9", None, None)
                .await
                .unwrap_err()
        ),
        MilestoneWriteRefusal::Denied
    );
    let v2 = db::milestones::create_milestone(pool, owner, repo, "v2", None, None)
        .await
        .unwrap();
    assert_eq!(v2.number, 2);
    db::milestones::set_issue_milestone(pool, owner, repo, first.id, Some(1))
        .await
        .unwrap();
    db::milestones::set_issue_milestone(pool, owner, repo, second.id, Some(1))
        .await
        .unwrap();
    let progress = db::milestones::get_milestone(pool, repo, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((progress.open_items, progress.closed_items), (2, 0));

    // Counts come from the issues that name the milestone, so closing one moves it.
    db::issues::update_issue(
        pool,
        second.id,
        DiscussionEdit {
            actor_id: owner,
            expected_state: "open",
            expected_updated: second.updated_at,
            title: None,
            body: None,
            state: Some("closed"),
        },
    )
    .await
    .unwrap()
    .expect("the issue closes");
    let progress = db::milestones::get_milestone(pool, repo, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((progress.open_items, progress.closed_items), (1, 1));

    let closed = db::milestones::set_milestone_state(pool, owner, repo, 2, "closed")
        .await
        .unwrap();
    assert!(closed.closed_at.is_some());
    let open = db::milestones::list_repo_milestones(pool, repo, Some("open"))
        .await
        .unwrap();
    assert_eq!(
        open.iter().map(|m| m.milestone.number).collect::<Vec<_>>(),
        vec![1]
    );
    db::milestones::set_milestone_state(pool, owner, repo, 2, "open")
        .await
        .unwrap();
    assert_eq!(
        milestone_refusal(
            db::milestones::update_milestone(pool, owner, repo, 2, "V1", None, None)
                .await
                .unwrap_err()
        ),
        MilestoneWriteRefusal::DuplicateTitle
    );
    db::milestones::update_milestone(pool, owner, repo, 2, "v2", Some("kept title"), None)
        .await
        .unwrap();

    // Issue filters: assignee, milestone, state, author and most-commented sort.
    assert_eq!(
        issue_set(
            pool,
            repo,
            ListFilter {
                assignee: Pick::Is(writer),
                ..Default::default()
            }
        )
        .await,
        vec![first.number]
    );
    assert_eq!(
        issue_set(
            pool,
            repo,
            ListFilter {
                assignee: Pick::Unset,
                ..Default::default()
            }
        )
        .await,
        vec![second.number]
    );
    assert_eq!(
        issue_set(
            pool,
            repo,
            ListFilter {
                milestone: Pick::Is(v1.id),
                ..Default::default()
            }
        )
        .await,
        vec![first.number, second.number]
    );
    assert_eq!(
        issue_set(
            pool,
            repo,
            ListFilter {
                milestone: Pick::Unset,
                ..Default::default()
            }
        )
        .await,
        Vec::<i32>::new()
    );
    assert_eq!(
        issue_set(
            pool,
            repo,
            ListFilter {
                state: Some("closed"),
                ..Default::default()
            }
        )
        .await,
        vec![second.number]
    );
    assert_eq!(
        issue_set(
            pool,
            repo,
            ListFilter {
                author_id: Some(reader),
                ..Default::default()
            }
        )
        .await,
        Vec::<i32>::new()
    );
    db::issues::add_issue_comment(pool, second.id, owner, "one")
        .await
        .unwrap();
    db::issues::add_issue_comment(pool, second.id, owner, "two")
        .await
        .unwrap();
    assert_eq!(
        issue_numbers(
            pool,
            repo,
            ListFilter {
                sort: ListSort::MostCommented,
                ..Default::default()
            }
        )
        .await,
        vec![second.number, first.number]
    );
    assert_eq!(
        issue_numbers(
            pool,
            repo,
            ListFilter {
                sort: ListSort::Oldest,
                ..Default::default()
            }
        )
        .await
        .len(),
        2
    );
    // Label names match ignoring case, and an issue must carry every name the filter gives.
    let bug = db::labels::create_label(pool, owner, repo, "bug", "#d73a4a", None)
        .await
        .unwrap();
    let ui = db::labels::create_label(pool, owner, repo, "UI", "#0075ca", None)
        .await
        .unwrap();
    db::labels::add_issue_labels(pool, owner, repo, first.id, &[bug.id, ui.id])
        .await
        .unwrap();
    db::labels::add_issue_labels(pool, owner, repo, second.id, &[bug.id])
        .await
        .unwrap();
    let bug_name = ["BUG".to_owned()];
    let bug_and_ui = ["bug".to_owned(), "ui".to_owned()];
    let missing = ["missing".to_owned()];
    assert_eq!(
        issue_set(
            pool,
            repo,
            ListFilter {
                labels: &bug_name,
                ..Default::default()
            }
        )
        .await,
        vec![first.number, second.number]
    );
    assert_eq!(
        issue_set(
            pool,
            repo,
            ListFilter {
                labels: &bug_and_ui,
                ..Default::default()
            }
        )
        .await,
        vec![first.number]
    );
    assert_eq!(
        issue_set(
            pool,
            repo,
            ListFilter {
                labels: &missing,
                ..Default::default()
            }
        )
        .await,
        Vec::<i32>::new()
    );

    assert_eq!(
        db::discussions::list_repo_authors(pool, repo)
            .await
            .unwrap(),
        vec!["alice"]
    );

    // Deleting a milestone clears it from its issues; it is gone for the next writes.
    db::milestones::delete_milestone(pool, owner, repo, 1)
        .await
        .unwrap();
    let cleared = db::issues::get_issue(pool, repo, first.number)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cleared.milestone_id, None);
    assert!(db::milestones::get_milestone(pool, repo, 1)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        milestone_refusal(
            db::milestones::set_issue_milestone(pool, owner, repo, first.id, Some(1))
                .await
                .unwrap_err()
        ),
        MilestoneWriteRefusal::NotFound
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_milestones_and_assignees_hold_their_rules() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_milestones_and_assignees_hold_their_rules() {
    contract(true).await;
}

/// Racing assignments cannot pass the limit. Each one counts the assignees before it inserts, so
/// without the item row lock every racer could count nine and insert its own tenth.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_concurrent_assignments_stop_at_the_limit() {
    let mut f = Database::new(true).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let pool = f.pool().clone();
    let repo = lifecycle::provision(
        (&pool, None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap()
    .id;
    let item = AssignedItem::Issue(
        db::issues::create_issue(&pool, repo, owner, "Racing", "")
            .await
            .unwrap()
            .id,
    );
    let mut members = Vec::new();
    for index in 0..17 {
        let member = f.actor(&format!("racer{index}")).await;
        db::repositories::access::grant_repo_access(&pool, member, repo, AccessLevel::Read)
            .await
            .unwrap();
        members.push(member);
    }
    for &member in &members[..9] {
        db::assignees::assign_user(&pool, owner, repo, item, member)
            .await
            .unwrap();
    }

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = tokio::task::JoinSet::new();
    for &member in &members[9..] {
        let (pool, barrier) = (pool.clone(), barrier.clone());
        tasks.spawn(async move {
            barrier.wait().await;
            db::assignees::assign_user(&pool, owner, repo, item, member).await
        });
    }
    let mut added = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(true) => added += 1,
            Ok(false) => panic!("a new assignee was reported as already assigned"),
            Err(error) => assert_eq!(assignment_refusal(error), AssignmentRefusal::TooMany),
        }
    }
    assert_eq!(added, 1);
    assert_eq!(
        db::assignees::list_assignees(&pool, item)
            .await
            .unwrap()
            .len(),
        10
    );
    f.finish().await;
}
