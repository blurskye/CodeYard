//! A refresh that started while a pull request was open still reads the fork after the pull
//! request closed. The closed pull request takes nothing from the fork, and an open one in the
//! same fork still follows it. A pull ref that got ahead of its row, because an earlier refresh
//! lost the row update, is recorded before a newer head is published.
use super::{refresh_one, revisions, ListedRefs};
use crate::backend::ci::{CiConfig, CiRuns};
use crate::backend::db;
use crate::backend::db::models::{PullRequest, Repository};
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::repositories::lifecycle;
use crate::backend::repositories::pull_requests::mutations;
use crate::backend::repositories::store::repo_store;
use std::sync::Arc;
use tokio::sync::Notify;

/// A base repository owned by `alice` with `main` on it, and a fork of it owned by `bob`.
struct Forks {
    _root: tempfile::TempDir,
    pool: db::DbPool,
    ci: CiRuns,
    broadcaster: Arc<NotificationBroadcaster>,
    alice: i64,
    bob: i64,
    base: Repository,
    fork: Repository,
    main: git2::Oid,
}

/// Commits `content` as `file` on `branch`, on top of `parent`.
fn commit(
    repo: &git2::Repository,
    branch: &str,
    parent: Option<git2::Oid>,
    file: &str,
    content: &str,
) -> git2::Oid {
    let parent = parent.map(|oid| repo.find_commit(oid).unwrap());
    let base = parent.as_ref().map(|commit| commit.tree().unwrap());
    let mut tree = repo.treebuilder(base.as_ref()).unwrap();
    let blob = repo.blob(content.as_bytes()).unwrap();
    tree.insert(file, blob, 0o100644).unwrap();
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let author = git2::Signature::now("Tester", "tester@example.invalid").unwrap();
    repo.commit(
        Some(&format!("refs/heads/{branch}")),
        &author,
        &author,
        file,
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

async fn forks() -> Forks {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("application.sqlite");
    let pool = db::init_db(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    db::run_migrations(&pool).await.unwrap();
    let alice = db::users::accounts::create_user(&pool, "alice", None)
        .await
        .unwrap();
    let bob = db::users::accounts::create_user(&pool, "bob", None)
        .await
        .unwrap();
    let repos = root.path().join("repos");
    let base = lifecycle::provision(
        (&pool, None),
        &repos,
        db::repositories::creation::RepositoryCreationOwner::personal(alice.id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let bare = git2::Repository::open_bare(&base.disk_path).unwrap();
    let main = commit(&bare, "main", None, "README.md", "# proj\n");
    let broadcaster = Arc::new(NotificationBroadcaster::new(&pool));
    let fork = lifecycle::fork(&pool, None, &repos, &broadcaster, bob.id, base.id, "alice")
        .await
        .unwrap();
    let ci = CiRuns::new(
        pool.clone(),
        Arc::new(CiConfig::default()),
        Arc::new(Notify::new()),
        None,
        broadcaster.clone(),
    );
    Forks {
        _root: root,
        pool,
        ci,
        broadcaster,
        alice: alice.id,
        bob: bob.id,
        base,
        fork,
        main,
    }
}

/// The commit that `refs/pull/<number>/head` names in the base repository.
fn pull_head(base: &Repository, number: i32) -> git2::Oid {
    let bare = git2::Repository::open_bare(&base.disk_path).unwrap();
    let reference = bare
        .find_reference(&format!("refs/pull/{number}/head"))
        .unwrap();
    reference.target().unwrap()
}

async fn row(forks: &Forks, id: i64) -> PullRequest {
    db::pull_requests::records::get_pull_request_by_id(&forks.pool, id)
        .await
        .unwrap()
        .unwrap()
}

/// Opens a pull request from `head_branch` on the fork into `main` of the base.
async fn open(forks: &Forks, head_branch: &str) -> PullRequest {
    let request = mutations::CreatePullRequestRequest {
        head_repo_id: Some(forks.fork.id),
        title: format!("From {head_branch}"),
        body: None,
        head_branch: head_branch.to_owned(),
        base_branch: "main".to_owned(),
        is_draft: false,
    };
    mutations::create_pull_request(
        &forks.pool,
        &forks.ci,
        &forks.broadcaster,
        forks.bob,
        "alice".to_owned(),
        "proj".to_owned(),
        request,
    )
    .await
    .unwrap()
}

/// Closes the pull request through the same conditional update the API uses.
async fn close(forks: &Forks, pull: &PullRequest) {
    let edit = mutations::UpdatePullRequestRequest {
        expected_state: "open".to_owned(),
        expected_updated: pull.updated_at.to_rfc3339(),
        title: None,
        body: None,
        state: Some("closed".to_owned()),
        base: None,
    };
    mutations::update_pull_request(
        &forks.pool,
        &forks.ci,
        &forks.broadcaster,
        forks.alice,
        ("alice".to_owned(), "proj".to_owned(), pull.number),
        edit,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn refresh_leaves_a_closed_pull_request_alone_and_still_follows_an_open_one() {
    let forks = forks().await;
    let fork = git2::Repository::open_bare(&forks.fork.disk_path).unwrap();
    let base = git2::Repository::open_bare(&forks.base.disk_path).unwrap();

    let feature = commit(&fork, "feature", Some(forks.main), "a.txt", "reviewed\n");
    let closed = open(&forks, "feature").await;
    let captured = revisions(row(&forks, closed.id).await).unwrap();
    close(&forks, &closed).await;
    let closed_row = row(&forks, closed.id).await;
    assert_eq!(closed_row.state, "closed");
    let moved = commit(&fork, "feature", Some(feature), "b.txt", "later\n");

    let changed = refresh_one(
        &forks.pool,
        &forks.fork,
        &forks.ci,
        forks.bob,
        &mut ListedRefs::new(),
        captured,
        Some(crate::backend::db::models::AccessLevel::Owner),
    )
    .await
    .unwrap();
    assert!(!changed);
    assert_eq!(pull_head(&forks.base, closed.number), feature);
    assert!(
        base.find_commit(moved).is_err(),
        "the closed pull request fetched the fork's new commit"
    );
    let after = serde_json::to_value(row(&forks, closed.id).await).unwrap();
    assert_eq!(after, serde_json::to_value(&closed_row).unwrap());

    let topic = commit(&fork, "topic", Some(forks.main), "c.txt", "first\n");
    let open_pull = open(&forks, "topic").await;
    let captured = revisions(row(&forks, open_pull.id).await).unwrap();
    let topic_moved = commit(&fork, "topic", Some(topic), "d.txt", "second\n");
    let changed = refresh_one(
        &forks.pool,
        &forks.fork,
        &forks.ci,
        forks.bob,
        &mut ListedRefs::new(),
        captured,
        Some(crate::backend::db::models::AccessLevel::Owner),
    )
    .await
    .unwrap();
    assert!(changed);
    assert_eq!(
        row(&forks, open_pull.id).await.head_sha,
        topic_moved.to_string()
    );
    assert_eq!(pull_head(&forks.base, open_pull.number), topic_moved);
}

#[tokio::test]
async fn refresh_recovers_a_published_head_after_the_fork_is_deleted() {
    let forks = forks().await;
    let fork = git2::Repository::open_bare(&forks.fork.disk_path).unwrap();
    let first = commit(&fork, "feature", Some(forks.main), "a.txt", "first\n");
    let pull = open(&forks, "feature").await;
    let second = commit(&fork, "feature", Some(first), "b.txt", "second\n");
    repo_store(&forks.base, None)
        .fetch_pull_head(
            &forks.pool,
            forks.base.id,
            forks.fork.id,
            forks.bob,
            &repo_store(&forks.fork, None),
            pull.number,
            &second.to_string(),
            Some(&first.to_string()),
        )
        .await
        .unwrap();
    let db::DbPool::Sqlite(pool) = &forks.pool else {
        unreachable!()
    };
    sqlx::query("DELETE FROM repositories WHERE id = ?1")
        .bind(forks.fork.id)
        .execute(pool)
        .await
        .unwrap();
    let captured = revisions(row(&forks, pull.id).await).unwrap();
    assert_eq!(captured.head_repo_id, None);
    assert!(refresh_one(
        &forks.pool,
        &forks.base,
        &forks.ci,
        forks.alice,
        &mut ListedRefs::new(),
        captured,
        Some(crate::backend::db::models::AccessLevel::Owner)
    )
    .await
    .unwrap());
    assert_eq!(row(&forks, pull.id).await.head_sha, second.to_string());
}

#[tokio::test]
async fn creation_catches_a_head_push_that_finished_after_the_initial_ref_read() {
    let forks = forks().await;
    let fork = git2::Repository::open_bare(&forks.fork.disk_path).unwrap();
    let first = commit(&fork, "feature", Some(forks.main), "a.txt", "first\n");
    let lock = crate::backend::git::locks::lock(std::path::Path::new(&forks.base.disk_path)).await;
    let create = open(&forks, "feature");
    let push = async {
        loop {
            if db::pull_requests::records::get_pull_request(&forks.pool, forks.base.id, 1)
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let second = commit(&fork, "feature", Some(first), "b.txt", "second\n");
        drop(lock);
        second
    };
    let (created, second) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(create, push)
    })
    .await
    .unwrap();
    assert_eq!(created.head_sha, second.to_string());
    assert_eq!(pull_head(&forks.base, created.number), second);
}

#[tokio::test]
async fn existing_owner_requests_do_not_take_a_write_lock() {
    let forks = forks().await;
    db::repositories::access::grant_repo_access(
        &forks.pool,
        forks.bob,
        forks.base.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let base = git2::Repository::open_bare(&forks.base.disk_path).unwrap();
    let main = {
        let parent = base.find_commit(forks.main).unwrap();
        let mut github = base.treebuilder(None).unwrap();
        github
            .insert("CODEOWNERS", base.blob(b"* @bob\n").unwrap(), 0o100644)
            .unwrap();
        let mut root = base.treebuilder(Some(&parent.tree().unwrap())).unwrap();
        root.insert(".github", github.write().unwrap(), 0o040000)
            .unwrap();
        let tree = base.find_tree(root.write().unwrap()).unwrap();
        let who = git2::Signature::now("alice", "alice@example.invalid").unwrap();
        base.commit(
            Some("refs/heads/main"),
            &who,
            &who,
            "Owners",
            &tree,
            &[&parent],
        )
        .unwrap()
    };
    commit(&base, "feature", Some(main), "a.txt", "change\n");
    let pull = mutations::create_pull_request(
        &forks.pool,
        &forks.ci,
        &forks.broadcaster,
        forks.alice,
        "alice".into(),
        "proj".into(),
        mutations::CreatePullRequestRequest {
            head_repo_id: Some(forks.base.id),
            title: "Owned".into(),
            body: None,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            is_draft: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        db::pull_requests::reviews::list_pull_request_reviewers(&forks.pool, pull.id)
            .await
            .unwrap()
            .len(),
        1
    );
    let db::DbPool::Sqlite(pool) = &forks.pool else {
        unreachable!()
    };
    let lock = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        crate::backend::repositories::pull_requests::code_owners::request_code_owner_reviews(
            &forks.pool,
            &forks.ci,
            &forks.base,
            &pull,
            forks.alice,
        ),
    )
    .await
    .expect("existing requests should need only reads");
    lock.rollback().await.unwrap();
}

#[tokio::test]
async fn refresh_records_a_published_head_before_publishing_a_newer_one() {
    let forks = forks().await;
    let fork = git2::Repository::open_bare(&forks.fork.disk_path).unwrap();

    let first = commit(&fork, "feature", Some(forks.main), "a.txt", "first\n");
    let pull = open(&forks, "feature").await;
    // Publication reaches `second`, but the row update that records it is lost.
    let second = commit(&fork, "feature", Some(first), "b.txt", "second\n");
    repo_store(&forks.base, None)
        .fetch_pull_head(
            &forks.pool,
            forks.base.id,
            forks.fork.id,
            forks.bob,
            &repo_store(&forks.fork, None),
            pull.number,
            &second.to_string(),
            Some(&first.to_string()),
        )
        .await
        .unwrap();
    assert_eq!(row(&forks, pull.id).await.head_sha, first.to_string());

    // The fork moves on before the next refresh.
    let third = commit(&fork, "feature", Some(second), "c.txt", "third\n");
    let captured = revisions(row(&forks, pull.id).await).unwrap();
    let changed = refresh_one(
        &forks.pool,
        &forks.fork,
        &forks.ci,
        forks.bob,
        &mut ListedRefs::new(),
        captured,
        Some(crate::backend::db::models::AccessLevel::Owner),
    )
    .await
    .unwrap();
    assert!(changed);
    assert_eq!(row(&forks, pull.id).await.head_sha, third.to_string());
    assert_eq!(pull_head(&forks.base, pull.number), third);
}
