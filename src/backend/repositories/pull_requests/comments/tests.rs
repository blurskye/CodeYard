use super::in_pending_review;
use crate::backend::db;
use crate::backend::db::pull_requests::{records, reviews};

#[tokio::test]
async fn discarded_review_cannot_authorize_comment_publication() {
    let root = tempfile::tempdir().unwrap();
    let pool = db::init_db(&format!(
        "sqlite://{}",
        root.path().join("app.db").display()
    ))
    .await
    .unwrap();
    db::run_migrations(&pool).await.unwrap();
    let user = db::users::accounts::create_user(&pool, "alice", None)
        .await
        .unwrap();
    let repo = crate::backend::repositories::lifecycle::provision(
        (&pool, None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(user.id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let head = "a".repeat(40);
    let pull = records::create_pull_request(
        &pool,
        records::NewPullRequest {
            repo_id: repo.id,
            head_repo_id: Some(repo.id),
            author_id: user.id,
            title: "Review",
            body: "",
            head_branch: "topic",
            head_sha: &head,
            base_branch: "main",
            base_sha: &head,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let comment = reviews::add_pending_pull_request_comment(
        &pool,
        reviews::NewPullRequestComment {
            pr_id: pull.id,
            author_id: user.id,
            body: "Private draft",
            path: Some("file"),
            line: Some(1),
            side: Some("RIGHT"),
            commit_sha: Some(&head),
            in_reply_to_comment_id: None,
            expected_revisions: None,
        },
    )
    .await
    .unwrap();
    assert!(in_pending_review(&pool, &comment).await.unwrap());
    assert!(reviews::discard_pending_review(&pool, pull.id, user.id)
        .await
        .unwrap());
    assert!(reviews::get_pull_request_comment_by_id(&pool, comment.id)
        .await
        .unwrap()
        .is_none());
    // Creation still holds its insertion result after the other request's discard.
    assert!(in_pending_review(&pool, &comment).await.unwrap());

    let submitted = reviews::add_pending_pull_request_comment(
        &pool,
        reviews::NewPullRequestComment {
            pr_id: pull.id,
            author_id: user.id,
            body: "Submitted comment",
            path: Some("file"),
            line: Some(1),
            side: Some("RIGHT"),
            commit_sha: Some(&head),
            in_reply_to_comment_id: None,
            expected_revisions: None,
        },
    )
    .await
    .unwrap();
    reviews::add_pull_request_review(&pool, pull.id, user.id, "commented", None, Some(&head))
        .await
        .unwrap();
    assert!(!in_pending_review(&pool, &submitted).await.unwrap());
    pool.close().await;
}
