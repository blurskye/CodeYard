use super::{db, json, protection::call, request, Router, Value};

pub(super) async fn run(
    router: &Router,
    scope: (&codeyard_db_core::DbPool, i64, i64),
    base: &str,
    tokens: (&str, &str, &str),
    sha: &str,
) {
    let (pool, repo, actor) = scope;
    let (writer, reader, outsider) = tokens;
    let branch = "review-policy";
    call(
        router,
        "POST",
        &format!("{base}/git/refs"),
        writer,
        json!({"ref":format!("refs/heads/{branch}"),"sha":sha}),
        ("git/create-ref", 201),
    )
    .await;
    codeyard_db_repos::repositories::protection::upsert_branch_protection_rule(
        pool,
        codeyard_db_repos::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: actor,
            repo_id: repo,
            pattern: branch,
            required_approving_reviews: 3,
            dismiss_stale_reviews: true,
            require_code_owner_review: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let path = format!("{base}/branches/{branch}/protection/required_pull_request_reviews");
    let review = call(
        router,
        "GET",
        &path,
        reader,
        Value::Null,
        ("repos/get-pull-request-review-protection", 200),
    )
    .await;
    assert_eq!(review["required_approving_review_count"], 3);
    assert_eq!(review["dismiss_stale_reviews"], true);
    assert_eq!(review["require_code_owner_reviews"], true);
    for token in [reader, writer] {
        let (status, _, body) = request(router, "HEAD", &path, Some(token), Value::Null).await;
        assert_eq!(status.as_u16(), 200);
        assert_eq!(body, Value::Null);
    }
    assert_eq!(
        call(
            router,
            "GET",
            &path,
            reader,
            Value::Null,
            ("repos/get-pull-request-review-protection", 200)
        )
        .await,
        review
    );
    call(
        router,
        "DELETE",
        &path,
        reader,
        Value::Null,
        ("repos/delete-pull-request-review-protection", 403),
    )
    .await;
    call(
        router,
        "DELETE",
        &path,
        outsider,
        Value::Null,
        ("repos/delete-pull-request-review-protection", 404),
    )
    .await;
    let original = call(
        router,
        "GET",
        &format!("{base}/git/commits/{sha}"),
        reader,
        Value::Null,
        ("git/get-commit", 200),
    )
    .await;
    let candidate = call(router, "POST", &format!("{base}/git/commits"), writer, json!({"message":"After removing review requirements","tree":original["tree"]["sha"],"parents":[sha]}), ("git/create-commit", 201)).await;
    let reference = format!("{base}/git/refs/heads/{branch}");
    call(
        router,
        "PATCH",
        &reference,
        writer,
        json!({"sha":candidate["sha"]}),
        ("git/update-ref", 403),
    )
    .await;
    call(
        router,
        "DELETE",
        &path,
        writer,
        Value::Null,
        ("repos/delete-pull-request-review-protection", 204),
    )
    .await;
    call(
        router,
        "GET",
        &path,
        reader,
        Value::Null,
        ("repos/get-pull-request-review-protection", 404),
    )
    .await;
    call(
        router,
        "DELETE",
        &path,
        writer,
        Value::Null,
        ("repos/delete-pull-request-review-protection", 404),
    )
    .await;
    let native =
        codeyard_db_repos::repositories::protection::list_branch_protection_rules(pool, repo)
            .await
            .unwrap()
            .into_iter()
            .find(|rule| rule.pattern == branch)
            .unwrap();
    assert!(!native.require_pull_request);
    assert_eq!(native.required_approving_reviews, 0);
    assert!(!native.require_code_owner_review);
    assert!(!native.dismiss_stale_reviews);
    assert!(native.enforce_admins && native.block_force_pushes && native.block_deletions);
    call(
        router,
        "PATCH",
        &reference,
        writer,
        json!({"sha":candidate["sha"]}),
        ("git/update-ref", 200),
    )
    .await;
    call(
        router,
        "DELETE",
        &format!("{base}/branches/{branch}/protection"),
        writer,
        Value::Null,
        ("repos/delete-branch-protection", 204),
    )
    .await;
    call(
        router,
        "DELETE",
        &reference,
        writer,
        Value::Null,
        ("git/delete-ref", 204),
    )
    .await;
}
