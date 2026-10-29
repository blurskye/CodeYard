use super::{assert_github_schema, db, json, request, Client, Router, Value};

pub(super) async fn call(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
    expected: (&str, u16),
) -> Value {
    let (status, _, body) = request(router, method, path, Some(token), body).await;
    assert_eq!(
        status.as_u16(),
        expected.1,
        "{} {method} {path}: {body}",
        expected.0
    );
    if expected.1 == 204 {
        assert_eq!(body, Value::Null);
    } else {
        assert_github_schema(expected.0, expected.1, &body);
    }
    body
}

async fn branch_operations_contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = crate::support::database::Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await;
    let other = Client::login(&router, f.pool(), bob).await;
    let mut tokens = Vec::new();
    for (actor, session, name, scope) in [
        (alice, &session.session, "branch writer", "write"),
        (alice, &session.session, "branch reader", "read"),
        (bob, &other.session, "branch outsider", "write"),
    ] {
        let token = codeyard::backend::identity::tokens::create(
            f.pool(),
            actor,
            Some(session),
            codeyard::backend::identity::tokens::TokenInput {
                name,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        tokens.push(format!("Bearer {}", token.value));
    }
    let base = "/api/v3/repos/alice/proj";
    let saved = call(
        &router,
        "PUT",
        &format!("{base}/contents/seed.txt"),
        &tokens[0],
        json!({"message":"Seed branch operations","content":"c2VlZAo="}),
        ("repos/create-or-update-file-contents", 201),
    )
    .await;
    let sha = saved["commit"]["sha"].as_str().unwrap();
    extended(
        &router,
        (f.pool(), repo.id, alice),
        base,
        (&tokens[0], &tokens[1], &tokens[2]),
        sha,
        &session,
    )
    .await;
    super::comparison::run(
        &router,
        (f.pool(), repo.id, alice, bob),
        base,
        (&tokens[0], &tokens[1], &tokens[2]),
        sha,
        &session,
    )
    .await;
    super::branch_admission::run(
        &router,
        (f.pool(), repo.id, alice),
        base,
        (&tokens[0], &tokens[1], &tokens[2]),
        sha,
    )
    .await;
}

#[tokio::test]
async fn sqlite_github_branch_operations() {
    branch_operations_contract(false).await;
}

#[tokio::test]
async fn postgres_github_branch_operations() {
    branch_operations_contract(true).await;
}

async fn extended(
    router: &Router,
    scope: (&db::DbPool, i64, i64),
    base: &str,
    tokens: (&str, &str, &str),
    sha: &str,
    session: &Client,
) {
    let (pool, repo, actor) = scope;
    let (writer, reader, outsider) = tokens;
    let app = db::github_apps::register(
        pool,
        actor,
        &session.session,
        &db::github_apps::App {
            id: 0,
            owner_id: actor,
            bot_id: 0,
            is_public: true,
            client_id: "protection-app-client".into(),
            slug: "protection-app".into(),
            name: "Protection App".into(),
            description: "Branch restriction fixture".into(),
            homepage: "https://example.test".into(),
            public_key: "fixture".into(),
            permissions: "{\"contents\":\"write\"}".into(),
            events: String::new(),
            access_level: 3,
            created_at: chrono::Utc::now(),
        },
        "fixture-secret-hash",
    )
    .await
    .unwrap()
    .unwrap();
    db::github_apps::install(
        pool,
        actor,
        &session.session,
        app.id,
        Some(actor),
        None,
        Some(&[repo]),
    )
    .await
    .unwrap()
    .unwrap();
    call(
        router,
        "POST",
        &format!("{base}/git/refs"),
        writer,
        json!({"ref":"refs/heads/rest-policy","sha":sha}),
        ("git/create-ref", 201),
    )
    .await;
    let path = format!("{base}/branches/rest-policy/protection");
    let input = json!({
        "required_status_checks":{"strict":true,"contexts":["lint"],"checks":[{"context":"ci","app_id":app.id}]},
        "enforce_admins":true,
        "required_pull_request_reviews":{"dismiss_stale_reviews":true,"required_approving_review_count":2,
            "require_code_owner_reviews":true,"require_last_push_approval":true,
            "dismissal_restrictions":{"users":["alice"],"apps":["protection-app"]},
            "bypass_pull_request_allowances":{"users":["alice"]}},
        "restrictions":{"users":["alice"],"teams":[],"apps":["protection-app"]},
        "required_linear_history":true,"required_conversation_resolution":true,
        "block_creations":true,"lock_branch":true,"allow_fork_syncing":true
    });
    call(
        router,
        "PUT",
        &path,
        reader,
        input.clone(),
        ("repos/update-branch-protection", 403),
    )
    .await;
    call(
        router,
        "PUT",
        &path,
        outsider,
        input.clone(),
        ("repos/update-branch-protection", 404),
    )
    .await;
    call(
        router,
        "PUT",
        &path,
        writer,
        json!({}),
        ("repos/update-branch-protection", 422),
    )
    .await;
    let saved = call(
        router,
        "PUT",
        &path,
        writer,
        input.clone(),
        ("repos/update-branch-protection", 200),
    )
    .await;
    assert_eq!(saved["lock_branch"]["enabled"], true);
    assert_eq!(saved["allow_fork_syncing"]["enabled"], true);
    assert_eq!(saved["block_creations"]["enabled"], true);
    assert_eq!(
        saved["required_status_checks"]["checks"][0]["app_id"],
        app.id
    );
    assert_eq!(saved["restrictions"]["apps"][0]["slug"], "protection-app");
    assert_eq!(
        saved["required_pull_request_reviews"]["dismissal_restrictions"]["users"][0]["login"],
        "alice"
    );
    assert_eq!(
        call(
            router,
            "GET",
            &path,
            reader,
            Value::Null,
            ("repos/get-branch-protection", 200)
        )
        .await,
        saved
    );
    let review_path = format!("{path}/required_pull_request_reviews");
    let reviews = call(router,"PATCH",&review_path,writer,json!({"required_approving_review_count":1,"dismissal_restrictions":{},"require_last_push_approval":false}), ("repos/update-pull-request-review-protection",200)).await;
    assert_eq!(reviews["required_approving_review_count"], 1);
    assert_eq!(reviews["dismiss_stale_reviews"], true);
    assert_eq!(reviews["require_code_owner_reviews"], true);
    assert_eq!(reviews["require_last_push_approval"], false);
    assert_eq!(reviews["dismissal_restrictions"]["users"], json!([]));
    assert_eq!(
        reviews["bypass_pull_request_allowances"]["users"][0]["login"],
        "alice"
    );
    call(
        router,
        "PATCH",
        &review_path,
        writer,
        json!({"required_approving_review_count":7}),
        ("repos/update-pull-request-review-protection", 422),
    )
    .await;
    call(
        router,
        "PATCH",
        &review_path,
        reader,
        json!({}),
        ("repos/update-pull-request-review-protection", 403),
    )
    .await;
    let signatures = format!("{path}/required_signatures");
    assert_eq!(
        call(
            router,
            "GET",
            &signatures,
            reader,
            Value::Null,
            ("repos/get-commit-signature-protection", 200)
        )
        .await["enabled"],
        false
    );
    call(
        router,
        "POST",
        &signatures,
        reader,
        Value::Null,
        ("repos/create-commit-signature-protection", 403),
    )
    .await;
    assert_eq!(
        call(
            router,
            "POST",
            &signatures,
            writer,
            Value::Null,
            ("repos/create-commit-signature-protection", 200)
        )
        .await["enabled"],
        true
    );
    let (status, _, body) = request(router, "HEAD", &signatures, Some(writer), Value::Null).await;
    assert_eq!(status.as_u16(), 200);
    assert_eq!(body, Value::Null);
    assert_eq!(
        call(
            router,
            "GET",
            &signatures,
            reader,
            Value::Null,
            ("repos/get-commit-signature-protection", 200)
        )
        .await["enabled"],
        true
    );
    assert_eq!(
        call(
            router,
            "PUT",
            &path,
            writer,
            input.clone(),
            ("repos/update-branch-protection", 200)
        )
        .await["required_signatures"]["enabled"],
        true
    );
    let apps_path = format!("{path}/restrictions/apps");
    let apps = call(
        router,
        "GET",
        &apps_path,
        reader,
        Value::Null,
        ("repos/get-apps-with-access-to-protected-branch", 200),
    )
    .await;
    assert_eq!(apps[0]["id"], app.id);
    assert_eq!(
        call(
            router,
            "POST",
            &apps_path,
            writer,
            json!({"apps":["protection-app","protection-app"]}),
            ("repos/add-app-access-restrictions", 200)
        )
        .await,
        apps
    );
    assert_eq!(
        call(
            router,
            "DELETE",
            &apps_path,
            writer,
            json!({"apps":["protection-app"]}),
            ("repos/remove-app-access-restrictions", 200)
        )
        .await,
        json!([])
    );
    assert_eq!(
        call(
            router,
            "PUT",
            &apps_path,
            writer,
            json!(["protection-app"]),
            ("repos/set-app-access-restrictions", 200)
        )
        .await,
        apps
    );
    call(
        router,
        "POST",
        &apps_path,
        writer,
        json!({"apps":["missing-app"]}),
        ("repos/add-app-access-restrictions", 422),
    )
    .await;
    call(
        router,
        "PUT",
        &apps_path,
        reader,
        json!({"apps":[]}),
        ("repos/set-app-access-restrictions", 403),
    )
    .await;
    let rules_path = format!("{base}/rules/branches/rest-policy");
    let rules = call(
        router,
        "GET",
        &rules_path,
        reader,
        Value::Null,
        ("repos/get-branch-rules", 200),
    )
    .await;
    for kind in [
        "deletion",
        "non_fast_forward",
        "pull_request",
        "required_status_checks",
        "required_signatures",
        "required_linear_history",
        "creation",
        "update",
    ] {
        assert!(
            rules
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule["type"] == kind),
            "{kind}: {rules}"
        );
    }
    call(
        router,
        "DELETE",
        &signatures,
        writer,
        Value::Null,
        ("repos/delete-commit-signature-protection", 204),
    )
    .await;
    assert_eq!(
        call(
            router,
            "GET",
            &signatures,
            reader,
            Value::Null,
            ("repos/get-commit-signature-protection", 200)
        )
        .await["enabled"],
        false
    );
    let renamed = call(
        router,
        "POST",
        &format!("{base}/branches/rest-policy/rename"),
        writer,
        json!({"new_name":"renamed-policy"}),
        ("repos/rename-branch", 201),
    )
    .await;
    assert_eq!(renamed["name"], "renamed-policy");
    assert_eq!(renamed["commit"]["sha"], sha);
    assert_eq!(renamed["protected"], true);
    call(
        router,
        "GET",
        &path,
        reader,
        Value::Null,
        ("repos/get-branch-protection", 404),
    )
    .await;
    let renamed_path = format!("{base}/branches/renamed-policy/protection");
    let policy = call(
        router,
        "GET",
        &renamed_path,
        reader,
        Value::Null,
        ("repos/get-branch-protection", 200),
    )
    .await;
    assert_eq!(policy["restrictions"]["apps"], apps);
    call(
        router,
        "POST",
        &format!("{base}/branches/renamed-policy/rename"),
        reader,
        json!({"new_name":"denied"}),
        ("repos/rename-branch", 403),
    )
    .await;
    call(
        router,
        "POST",
        &format!("{base}/branches/renamed-policy/rename"),
        writer,
        json!({"new_name":"main"}),
        ("repos/rename-branch", 422),
    )
    .await;
    call(
        router,
        "POST",
        &format!("{base}/branches/renamed-policy/rename"),
        writer,
        json!({"new_name":"../bad"}),
        ("repos/rename-branch", 422),
    )
    .await;
    call(
        router,
        "POST",
        &format!("{base}/branches/main/rename"),
        writer,
        json!({"new_name":"trunk"}),
        ("repos/rename-branch", 201),
    )
    .await;
    assert_eq!(
        call(router, "GET", base, reader, Value::Null, ("repos/get", 200)).await["default_branch"],
        "trunk"
    );
    call(
        router,
        "POST",
        &format!("{base}/branches/trunk/rename"),
        writer,
        json!({"new_name":"main"}),
        ("repos/rename-branch", 201),
    )
    .await;
    call(
        router,
        "DELETE",
        &renamed_path,
        writer,
        Value::Null,
        ("repos/delete-branch-protection", 204),
    )
    .await;
    assert_eq!(
        call(
            router,
            "GET",
            &format!("{base}/rules/branches/renamed-policy"),
            reader,
            Value::Null,
            ("repos/get-branch-rules", 200)
        )
        .await,
        json!([])
    );
    call(
        router,
        "DELETE",
        &format!("{base}/git/refs/heads/renamed-policy"),
        writer,
        Value::Null,
        ("git/delete-ref", 204),
    )
    .await;
}

pub(super) async fn run(
    router: &Router,
    resource: (&db::DbPool, i64, i64),
    base: &str,
    tokens: (&str, &str, &str),
    sha: &str,
) {
    let (pool, repo_id, actor_id) = resource;
    let (writer, reader, outsider) = tokens;
    call(
        router,
        "POST",
        &format!("{base}/git/refs"),
        writer,
        json!({"ref":"refs/heads/policy", "sha":sha}),
        ("git/create-ref", 201),
    )
    .await;
    db::repositories::protection::upsert_branch_protection_rule(
        pool,
        db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id,
            repo_id,
            pattern: "policy",
            require_status_checks: true,
            required_status_checks: vec!["seed".into()],
            require_up_to_date_branch: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let path = format!("{base}/branches/policy/protection");
    let policy = call(
        router,
        "GET",
        &path,
        reader,
        Value::Null,
        ("repos/get-branch-protection", 200),
    )
    .await;
    assert_eq!(
        policy["required_status_checks"]["contexts"],
        json!(["seed"])
    );
    assert_eq!(policy["enforce_admins"]["enabled"], true);
    for suffix in [
        "",
        "/enforce_admins",
        "/required_status_checks",
        "/required_status_checks/contexts",
    ] {
        for token in [reader, writer] {
            let (status, _, body) = request(
                router,
                "HEAD",
                &format!("{path}{suffix}"),
                Some(token),
                json!(["seed"]),
            )
            .await;
            assert_eq!(status.as_u16(), 200, "HEAD {suffix}: {body}");
            assert_eq!(body, Value::Null);
        }
    }
    assert_eq!(
        call(
            router,
            "GET",
            &path,
            reader,
            Value::Null,
            ("repos/get-branch-protection", 200)
        )
        .await,
        policy,
        "HEAD must not change administrators or required checks"
    );
    call(
        router,
        "GET",
        &path,
        outsider,
        Value::Null,
        ("repos/get-branch-protection", 404),
    )
    .await;
    call(
        router,
        "GET",
        &format!("{base}/branches/missing/protection"),
        reader,
        Value::Null,
        ("repos/get-branch-protection", 404),
    )
    .await;
    let admin = format!("{path}/enforce_admins");
    call(
        router,
        "POST",
        &admin,
        reader,
        Value::Null,
        ("repos/set-admin-branch-protection", 403),
    )
    .await;
    assert_eq!(
        call(
            router,
            "GET",
            &admin,
            reader,
            Value::Null,
            ("repos/get-admin-branch-protection", 200)
        )
        .await["enabled"],
        true
    );

    // The admin switch changes real ref admission, not just its REST document.
    let commit = call(
        router,
        "GET",
        &format!("{base}/git/commits/{sha}"),
        reader,
        Value::Null,
        ("git/get-commit", 200),
    )
    .await;
    let child = call(
        router,
        "POST",
        &format!("{base}/git/commits"),
        writer,
        json!({"message":"policy child", "parents":[sha], "tree":commit["tree"]["sha"]}),
        ("git/create-commit", 201),
    )
    .await;
    let update = json!({"sha":child["sha"]});
    let reference = format!("{base}/git/refs/heads/policy");
    call(
        router,
        "PATCH",
        &reference,
        writer,
        update.clone(),
        ("git/update-ref", 403),
    )
    .await;
    call(
        router,
        "DELETE",
        &admin,
        writer,
        Value::Null,
        ("repos/delete-admin-branch-protection", 204),
    )
    .await;
    assert_eq!(
        call(
            router,
            "GET",
            &admin,
            reader,
            Value::Null,
            ("repos/get-admin-branch-protection", 200)
        )
        .await["enabled"],
        false
    );
    call(
        router,
        "PATCH",
        &reference,
        writer,
        update,
        ("git/update-ref", 200),
    )
    .await;
    assert_eq!(
        call(
            router,
            "POST",
            &admin,
            writer,
            Value::Null,
            ("repos/set-admin-branch-protection", 200)
        )
        .await["enabled"],
        true
    );

    let checks = format!("{path}/required_status_checks");
    let policy = call(
        router,
        "GET",
        &checks,
        reader,
        Value::Null,
        ("repos/get-status-checks-protection", 200),
    )
    .await;
    assert_eq!(policy["strict"], true);
    call(
        router,
        "PATCH",
        &checks,
        reader,
        json!({"strict":false}),
        ("repos/update-status-check-protection", 403),
    )
    .await;
    call(
        router,
        "PATCH",
        &checks,
        writer,
        json!({"checks":[{"context":"ci","app_id":42}]}),
        ("repos/update-status-check-protection", 422),
    )
    .await;
    let policy = call(
        router,
        "PATCH",
        &checks,
        writer,
        json!({"strict":false,"checks":[{"context":"ci","app_id":-1}]}),
        ("repos/update-status-check-protection", 200),
    )
    .await;
    assert_eq!(policy["strict"], false);
    assert_eq!(policy["contexts"], json!(["ci"]));
    assert_eq!(policy["checks"], json!([{"context":"ci", "app_id":null}]));
    let contexts = format!("{checks}/contexts");
    assert_eq!(
        call(
            router,
            "GET",
            &contexts,
            reader,
            Value::Null,
            ("repos/get-all-status-check-contexts", 200)
        )
        .await,
        json!(["ci"])
    );
    assert_eq!(
        call(
            router,
            "POST",
            &contexts,
            writer,
            json!({"contexts":["lint","ci"]}),
            ("repos/add-status-check-contexts", 200)
        )
        .await,
        json!(["ci", "lint"])
    );
    assert_eq!(
        call(
            router,
            "PUT",
            &contexts,
            writer,
            json!(["unit"]),
            ("repos/set-status-check-contexts", 200)
        )
        .await,
        json!(["unit"])
    );
    assert_eq!(
        call(
            router,
            "DELETE",
            &contexts,
            writer,
            json!({"contexts":["missing","unit"]}),
            ("repos/remove-status-check-contexts", 200)
        )
        .await,
        json!([])
    );
    call(
        router,
        "POST",
        &contexts,
        writer,
        json!({"contexts":["x".repeat(513)]}),
        ("repos/add-status-check-contexts", 422),
    )
    .await;
    let (first, second) = tokio::join!(
        call(
            router,
            "POST",
            &contexts,
            writer,
            json!(["parallel-a"]),
            ("repos/add-status-check-contexts", 200)
        ),
        call(
            router,
            "POST",
            &contexts,
            writer,
            json!(["parallel-b"]),
            ("repos/add-status-check-contexts", 200)
        ),
    );
    assert!(first.as_array().unwrap().contains(&json!("parallel-a")));
    assert!(second.as_array().unwrap().contains(&json!("parallel-b")));
    assert_eq!(
        call(
            router,
            "GET",
            &contexts,
            reader,
            Value::Null,
            ("repos/get-all-status-check-contexts", 200)
        )
        .await,
        json!(["parallel-a", "parallel-b"])
    );
    call(
        router,
        "DELETE",
        &checks,
        writer,
        Value::Null,
        ("repos/remove-status-check-protection", 204),
    )
    .await;
    call(
        router,
        "GET",
        &checks,
        reader,
        Value::Null,
        ("repos/get-status-checks-protection", 404),
    )
    .await;
    call(
        router,
        "GET",
        &contexts,
        reader,
        Value::Null,
        ("repos/get-all-status-check-contexts", 404),
    )
    .await;
    call(
        router,
        "DELETE",
        &path,
        reader,
        Value::Null,
        ("repos/delete-branch-protection", 403),
    )
    .await;
    call(
        router,
        "DELETE",
        &path,
        writer,
        Value::Null,
        ("repos/delete-branch-protection", 204),
    )
    .await;
    call(
        router,
        "GET",
        &path,
        reader,
        Value::Null,
        ("repos/get-branch-protection", 404),
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
