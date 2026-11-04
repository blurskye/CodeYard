use super::{
    assert_github_schema, db, json, protection::call, request, Client, Router, StatusCode, Value,
};
use codeyard::backend::repositories::access::change_collaborator;
use codeyard_db_core::models::AccessLevel;

pub(super) async fn run(
    router: &Router,
    resource: (&codeyard_db_core::DbPool, i64, i64, i64),
    base: &str,
    tokens: (&str, &str, &str),
    sha: &str,
    session: &Client,
) {
    let (writer, reader, outsider) = tokens;
    let (pool, repo_id, alice, bob) = resource;
    let original = call(
        router,
        "GET",
        &format!("{base}/git/commits/{sha}"),
        reader,
        Value::Null,
        ("git/get-commit", 200),
    )
    .await;
    let tree = call(router, "POST", &format!("{base}/git/trees"), writer,
        json!({"base_tree":original["tree"]["sha"],"tree":[{"path":"comparison.txt","mode":"100644","type":"blob","content":"comparison marker\n"}]}), ("git/create-tree", 201)).await;
    let mut commits = Vec::new();
    let mut head = sha.to_owned();
    for message in ["Comparison one", "Comparison two", "Comparison three"] {
        let created = call(
            router,
            "POST",
            &format!("{base}/git/commits"),
            writer,
            json!({"tree":tree["sha"],"parents":[head],"message":message}),
            ("git/create-commit", 201),
        )
        .await;
        head = created["sha"].as_str().unwrap().to_owned();
        commits.push(head.clone());
    }
    let path = format!("{base}/compare/{sha}...{head}");
    let comparison = call(
        router,
        "GET",
        &path,
        reader,
        Value::Null,
        ("repos/compare-commits", 200),
    )
    .await;
    assert_eq!(comparison["status"], "ahead");
    assert_eq!(comparison["ahead_by"], 3);
    assert_eq!(comparison["behind_by"], 0);
    assert_eq!(comparison["total_commits"], 3);
    assert_eq!(comparison["merge_base_commit"]["sha"], sha);
    assert_eq!(comparison["base_commit"]["sha"], sha);
    assert_eq!(
        comparison["commits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|commit| commit["sha"].as_str().unwrap())
            .collect::<Vec<_>>(),
        commits.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(comparison["files"].as_array().unwrap().len(), 1);
    assert_eq!(comparison["files"][0]["filename"], "comparison.txt");
    assert!(comparison["files"][0]["patch"]
        .as_str()
        .unwrap()
        .contains("+comparison marker"));
    for page in 1..=3 {
        let (status, headers, document) = request(
            router,
            "GET",
            &format!("{path}?per_page=1&page={page}"),
            Some(reader),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{document}");
        assert_github_schema("repos/compare-commits", 200, &document);
        assert_eq!(document["commits"].as_array().unwrap().len(), 1);
        assert_eq!(document["commits"][0]["sha"], commits[page - 1]);
        assert_eq!(document["total_commits"], 3);
        assert_eq!(document.get("files").is_some(), page == 1);
        let link = headers["link"].to_str().unwrap();
        assert_eq!(link.contains("rel=\"next\""), page < 3);
        assert_eq!(link.contains("rel=\"prev\""), page > 1);
    }
    let beyond = call(
        router,
        "GET",
        &format!("{path}?per_page=1&page=999"),
        reader,
        Value::Null,
        ("repos/compare-commits", 200),
    )
    .await;
    assert_eq!(beyond["commits"], json!([]));
    assert_eq!(beyond["total_commits"], 3);
    let behind = call(
        router,
        "GET",
        &format!("{base}/compare/{head}...{sha}"),
        reader,
        Value::Null,
        ("repos/compare-commits", 200),
    )
    .await;
    assert_eq!(behind["status"], "behind");
    assert_eq!(behind["ahead_by"], 0);
    assert_eq!(behind["behind_by"], 3);
    assert_eq!(behind["commits"], json!([]));
    assert_eq!(behind["files"], json!([]));
    let identical = call(
        router,
        "GET",
        &format!("{base}/compare/{sha}...{sha}"),
        reader,
        Value::Null,
        ("repos/compare-commits", 200),
    )
    .await;
    assert_eq!(identical["status"], "identical");
    assert_eq!(identical["total_commits"], 0);
    assert_eq!(identical["files"], json!([]));
    call(
        router,
        "GET",
        &path,
        outsider,
        Value::Null,
        ("repos/compare-commits", 404),
    )
    .await;
    call(
        router,
        "GET",
        &format!("{base}/compare/missing...{sha}"),
        reader,
        Value::Null,
        ("repos/compare-commits", 404),
    )
    .await;
    let unrelated = call(
        router,
        "POST",
        &format!("{base}/git/commits"),
        writer,
        json!({"tree":tree["sha"],"parents":[],"message":"Unrelated root"}),
        ("git/create-commit", 201),
    )
    .await;
    call(
        router,
        "GET",
        &format!(
            "{base}/compare/{sha}...{}",
            unrelated["sha"].as_str().unwrap()
        ),
        reader,
        Value::Null,
        ("repos/compare-commits", 404),
    )
    .await;
    let permalink = url::Url::parse(comparison["permalink_url"].as_str().unwrap()).unwrap();
    let (status, page) = session.page(permalink.path()).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("comparison marker"));
    assert!(!page.contains("Create pull request"));
    call(
        router,
        "POST",
        &format!("{base}/git/refs"),
        writer,
        json!({"ref":"refs/heads/compare/topic","sha":head}),
        ("git/create-ref", 201),
    )
    .await;
    call(
        router,
        "GET",
        &format!("{base}/compare/main...compare%2Ftopic"),
        reader,
        Value::Null,
        ("repos/compare-commits", 200),
    )
    .await;
    call(
        router,
        "DELETE",
        &format!("{base}/git/refs/heads/compare%2Ftopic"),
        writer,
        Value::Null,
        ("git/delete-ref", 204),
    )
    .await;

    // A private fork is read independently, without importing its objects or weakening
    // permissions merely because both repositories belong to one network.
    change_collaborator(
        pool,
        alice,
        repo_id,
        "bob",
        Some(codeyard_db_core::models::AccessLevel::Read),
    )
    .await
    .unwrap();
    let forker = Client::login(router, pool, bob).await;
    let native = base.strip_prefix("/api/v3/repos/").unwrap();
    let fork = forker
        .expect(
            "POST",
            &format!("/api/repos/{native}/forks"),
            Value::Null,
            StatusCode::CREATED,
        )
        .await;
    let fork_id = fork["repo"]["id"].as_i64().unwrap();
    change_collaborator(pool, alice, repo_id, "bob", None)
        .await
        .unwrap();
    change_collaborator(
        pool,
        bob,
        fork_id,
        "alice",
        Some(codeyard_db_core::models::AccessLevel::Read),
    )
    .await
    .unwrap();
    let name = native.split('/').nth(1).unwrap();
    let fork_base = format!("/api/v3/repos/bob/{name}");
    let tree = call(router, "POST", &format!("{fork_base}/git/trees"), outsider,
        json!({"base_tree":original["tree"]["sha"],"tree":[{"path":"fork-marker.txt","mode":"100644","type":"blob","content":"private comparison marker\n"}]}), ("git/create-tree", 201)).await;
    let fork_head = call(
        router,
        "POST",
        &format!("{fork_base}/git/commits"),
        outsider,
        json!({"tree":tree["sha"],"parents":[sha],"message":"Private fork head"}),
        ("git/create-commit", 201),
    )
    .await;
    call(
        router,
        "PATCH",
        &format!("{fork_base}/git/refs/heads/main"),
        outsider,
        json!({"sha":fork_head["sha"]}),
        ("git/update-ref", 200),
    )
    .await;
    let fork_path = format!("{base}/compare/{head}...bob:main");
    let diverged = call(
        router,
        "GET",
        &fork_path,
        reader,
        Value::Null,
        ("repos/compare-commits", 200),
    )
    .await;
    assert_eq!(diverged["status"], "diverged");
    assert_eq!(diverged["ahead_by"], 1);
    assert_eq!(diverged["behind_by"], 3);
    assert_eq!(diverged["merge_base_commit"]["sha"], sha);
    assert_eq!(diverged["commits"][0]["sha"], fork_head["sha"]);
    assert!(diverged["files"][0]["patch"]
        .as_str()
        .unwrap()
        .contains("private comparison marker"));
    let permalink = url::Url::parse(diverged["permalink_url"].as_str().unwrap()).unwrap();
    let (status, page) = session.page(permalink.path()).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("private comparison marker"));
    assert!(!page.contains("Create pull request"));
    change_collaborator(pool, bob, fork_id, "alice", None)
        .await
        .unwrap();
    let denied = call(
        router,
        "GET",
        &fork_path,
        reader,
        Value::Null,
        ("repos/compare-commits", 404),
    )
    .await;
    assert!(!denied.to_string().contains("private comparison marker"));
    let (status, page) = session.page(permalink.path()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!page.contains("private comparison marker"));

    // GitHub documents this status without a response schema.
    let (status, _, body) = request(
        router,
        "POST",
        &format!("{base}/merge-upstream"),
        Some(writer),
        json!({"branch":"main"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["message"], "Repository is not a fork");
    assert_eq!(
        body["errors"],
        json!([{"code":"custom", "message":"Repository is not a fork"}])
    );
    call(
        router,
        "POST",
        &format!("{fork_base}/merge-upstream"),
        reader,
        json!({"branch":"main"}),
        ("repos/merge-upstream", 403),
    )
    .await;
    // A fork's writer cannot import objects from a parent that is no longer readable.
    call(
        router,
        "POST",
        &format!("{fork_base}/merge-upstream"),
        outsider,
        json!({"branch":"main"}),
        ("repos/merge-upstream", 404),
    )
    .await;
    change_collaborator(
        pool,
        alice,
        repo_id,
        "bob",
        Some(codeyard_db_core::models::AccessLevel::Read),
    )
    .await
    .unwrap();
    for (target, token) in [(base, writer), (&fork_base, outsider)] {
        call(
            router,
            "POST",
            &format!("{target}/git/refs"),
            token,
            json!({"ref":"refs/heads/upstream-sync","sha":sha}),
            ("git/create-ref", 201),
        )
        .await;
    }
    let upstream = call(router,"POST",&format!("{base}/git/commits"),writer,json!({"tree":original["tree"]["sha"],"parents":[sha],"message":"Upstream synchronization"}),("git/create-commit",201)).await;
    call(
        router,
        "PATCH",
        &format!("{base}/git/refs/heads/upstream-sync"),
        writer,
        json!({"sha":upstream["sha"]}),
        ("git/update-ref", 200),
    )
    .await;
    let merged = call(
        router,
        "POST",
        &format!("{fork_base}/merge-upstream"),
        outsider,
        json!({"branch":"upstream-sync"}),
        ("repos/merge-upstream", 200),
    )
    .await;
    assert_eq!(merged["merge_type"], "fast-forward");
    assert_eq!(merged["base_branch"], "alice:upstream-sync");
    assert_eq!(
        call(
            router,
            "GET",
            &format!("{fork_base}/branches/upstream-sync"),
            outsider,
            Value::Null,
            ("repos/get-branch", 200)
        )
        .await["commit"]["sha"],
        upstream["sha"]
    );
    assert_eq!(
        call(
            router,
            "POST",
            &format!("{fork_base}/merge-upstream"),
            outsider,
            json!({"branch":"upstream-sync"}),
            ("repos/merge-upstream", 200)
        )
        .await["merge_type"],
        "none"
    );
    call(
        router,
        "PATCH",
        &format!("{fork_base}/git/refs/heads/upstream-sync"),
        outsider,
        json!({"sha":fork_head["sha"],"force":true}),
        ("git/update-ref", 200),
    )
    .await;
    assert_eq!(
        call(
            router,
            "POST",
            &format!("{fork_base}/merge-upstream"),
            outsider,
            json!({"branch":"upstream-sync"}),
            ("repos/merge-upstream", 200),
        )
        .await["merge_type"],
        "merge"
    );
    let merged = call(
        router,
        "GET",
        &format!("{fork_base}/commits/upstream-sync"),
        outsider,
        Value::Null,
        ("repos/get-commit", 200),
    )
    .await;
    assert_eq!(merged["parents"][0]["sha"], fork_head["sha"]);
    assert_eq!(merged["parents"][1]["sha"], upstream["sha"]);
    call(
        router,
        "POST",
        &format!("{fork_base}/merge-upstream"),
        outsider,
        json!({"branch":"missing"}),
        ("repos/merge-upstream", 422),
    )
    .await;
    change_collaborator(pool, alice, repo_id, "bob", None)
        .await
        .unwrap();
    for (target, token) in [(base, writer), (&fork_base, outsider)] {
        call(
            router,
            "DELETE",
            &format!("{target}/git/refs/heads/upstream-sync"),
            token,
            Value::Null,
            ("git/delete-ref", 204),
        )
        .await;
    }
}
