use crate::backend::repositories::pull_requests::comments::in_pending_review;

#[tokio::test]
async fn discarded_review_cannot_authorize_comment_publication() {
    let root = tempfile::tempdir().unwrap();
    let pool = codeyard_db_core::init_db(&format!(
        "sqlite://{}",
        root.path().join("app.db").display()
    ))
    .await
    .unwrap();
    codeyard_db_core::run_migrations(&pool).await.unwrap();
    let user = codeyard_db_identity::users::accounts::create_user(&pool, "alice", None)
        .await
        .unwrap();
    let repo = crate::backend::repositories::lifecycle::provision(
        (&pool, None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(user.id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let head = "a".repeat(40);
    let pull = codeyard_db_repos::pull_requests::records::create_pull_request(
        &pool,
        codeyard_db_repos::pull_requests::records::NewPullRequest {
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
            maintainer_can_modify: false,
            issue: None,
        },
    )
    .await
    .unwrap();
    let comment = codeyard_db_repos::pull_requests::reviews::add_pending_pull_request_comment(
        &pool,
        codeyard_db_repos::pull_requests::reviews::NewPullRequestComment {
            pr_id: pull.id,
            author_id: user.id,
            body: "Private draft",
            start_line: None,
            start_side: None,
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
    assert!(
        codeyard_db_repos::pull_requests::reviews::discard_pending_review(&pool, pull.id, user.id)
            .await
            .unwrap()
    );
    assert!(
        codeyard_db_repos::pull_requests::reviews::get_pull_request_comment_by_id(
            &pool, comment.id
        )
        .await
        .unwrap()
        .is_none()
    );
    // Creation still holds its insertion result after the other request's discard.
    assert!(in_pending_review(&pool, &comment).await.unwrap());

    let submitted = codeyard_db_repos::pull_requests::reviews::add_pending_pull_request_comment(
        &pool,
        codeyard_db_repos::pull_requests::reviews::NewPullRequestComment {
            pr_id: pull.id,
            author_id: user.id,
            body: "Submitted comment",
            start_line: None,
            start_side: None,
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
    codeyard_db_repos::pull_requests::reviews::add_pull_request_review(
        &pool,
        pull.id,
        user.id,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &codeyard_db_identity::users::sessions::create_session(
                &pool,
                &uuid::Uuid::new_v4().simple().to_string().repeat(2),
                user.id,
                None,
                None,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .await
            .unwrap()
            .id,
        ),
        "commented",
        None,
        Some(&head),
    )
    .await
    .unwrap();
    assert!(!in_pending_review(&pool, &submitted).await.unwrap());
    pool.close().await;
}
