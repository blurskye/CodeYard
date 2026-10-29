use super::{db, json, protection::call, request, Router, Value};
use codeyard::backend::repositories::store::repo_store;

#[tokio::test]
async fn sqlite_app_access_restrictions() {
    app_contract(false).await;
}

#[tokio::test]
async fn postgres_app_access_restrictions() {
    app_contract(true).await;
}

async fn app_contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    use super::{tokens, Client, Database, TokenInput};
    use crate::support::organizations::applied;
    use axum::http::StatusCode;
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await;
    let org = applied(
        db::organizations::create_organization(
            f.pool(),
            alice,
            &db::organizations::NewOrganization::new(
                "apps-org",
                db::organizations::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    let repo = session
        .expect(
            "POST",
            "/api/repos",
            json!({"name":"policy","owner_org_id":org}),
            StatusCode::CREATED,
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let base = "/api/v3/repos/apps-org/policy";
    let write = tokens::create(
        f.pool(),
        alice,
        Some(&session.session),
        TokenInput {
            name: "writer",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let read = tokens::create(
        f.pool(),
        alice,
        Some(&session.session),
        TokenInput {
            name: "reader",
            scope: "read",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let writer = format!("Bearer {}", write.value);
    let reader = format!("Bearer {}", read.value);
    let seed = call(
        &router,
        "PUT",
        &format!("{base}/contents/seed"),
        &writer,
        json!({"message":"Seed","content":"c2VlZA=="}),
        ("repos/create-or-update-file-contents", 201),
    )
    .await;
    let sha = seed["commit"]["sha"].as_str().unwrap();
    let response = session
        .form_reply(
            "/settings/apps/new",
            &[
                ("name", "Policy App"),
                ("slug", "policy-app"),
                ("homepage", "https://example.test"),
                ("permissions", r#"{"contents":"write"}"#),
                ("events", "push"),
            ],
        )
        .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let app = db::github_apps::app(f.pool(), "policy-app")
        .await
        .unwrap()
        .unwrap();
    let path = format!("{base}/branches/main/protection");
    call(
        &router,
        "PUT",
        &path,
        &writer,
        json!({"required_status_checks":null,"enforce_admins":true,
        "required_pull_request_reviews":null,"restrictions":{"users":[],"teams":[],"apps":[]}}),
        ("repos/update-branch-protection", 200),
    )
    .await;
    let apps = format!("{path}/restrictions/apps");
    call(
        &router,
        "POST",
        &apps,
        &writer,
        json!(["policy-app"]),
        ("repos/add-app-access-restrictions", 422),
    )
    .await;
    let installation = db::github_apps::install(
        f.pool(),
        alice,
        &session.session,
        app.id,
        None,
        Some(org),
        Some(&[repo]),
    )
    .await
    .unwrap()
    .unwrap();
    call(
        &router,
        "POST",
        &apps,
        &reader,
        json!(["policy-app"]),
        ("repos/add-app-access-restrictions", 403),
    )
    .await;
    for bad in [
        json!(["missing"]),
        json!(["bad\0slug"]),
        json!(vec!["policy-app"; 101]),
    ] {
        call(
            &router,
            "PUT",
            &apps,
            &writer,
            bad,
            ("repos/set-app-access-restrictions", 422),
        )
        .await;
    }
    let added = call(
        &router,
        "POST",
        &apps,
        &writer,
        json!({"apps":["policy-app","policy-app"]}),
        ("repos/add-app-access-restrictions", 200),
    )
    .await;
    assert_eq!(added.as_array().unwrap().len(), 1);
    assert_eq!(added[0]["id"], app.id);
    assert_eq!(added[0]["slug"], "policy-app");
    assert_eq!(
        call(
            &router,
            "GET",
            &apps,
            &reader,
            Value::Null,
            ("repos/get-apps-with-access-to-protected-branch", 200)
        )
        .await,
        added
    );
    let original = call(
        &router,
        "GET",
        &format!("{base}/git/commits/{sha}"),
        &reader,
        Value::Null,
        ("git/get-commit", 200),
    )
    .await;
    let child = call(
        &router,
        "POST",
        &format!("{base}/git/commits"),
        &writer,
        json!({"message":"Allowed app push","tree":original["tree"]["sha"],"parents":[sha]}),
        ("git/create-commit", 201),
    )
    .await;
    let repository = db::repositories::records::get_repository_by_id(f.pool(), repo)
        .await
        .unwrap()
        .unwrap();
    let store = repo_store(&repository, None);
    store
        .update_reference_as(
            f.pool(),
            repo,
            app.bot_id,
            "refs/heads/main",
            child["sha"].as_str().unwrap(),
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        call(
            &router,
            "PUT",
            &apps,
            &writer,
            json!([]),
            ("repos/set-app-access-restrictions", 200)
        )
        .await,
        json!([])
    );
    assert!(store
        .update_reference_as(f.pool(), repo, app.bot_id, "refs/heads/main", sha, true)
        .await
        .unwrap_err()
        .is_protected_branch());
    call(
        &router,
        "POST",
        &apps,
        &writer,
        json!(["policy-app"]),
        ("repos/add-app-access-restrictions", 200),
    )
    .await;
    db::github_apps::change_installation(f.pool(), app.id, installation.id, "suspend")
        .await
        .unwrap();
    call(
        &router,
        "PUT",
        &apps,
        &writer,
        json!(["policy-app"]),
        ("repos/set-app-access-restrictions", 422),
    )
    .await;
    assert!(store
        .update_reference_as(f.pool(), repo, app.bot_id, "refs/heads/main", sha, true)
        .await
        .is_err());
    assert_eq!(
        call(
            &router,
            "DELETE",
            &apps,
            &writer,
            json!(["policy-app"]),
            ("repos/remove-app-access-restrictions", 200)
        )
        .await,
        json!([])
    );
}

pub(super) async fn run(
    router: &Router,
    resource: (&db::DbPool, i64, i64, i64),
    base: &str,
    tokens: (&str, &str, &str),
    sha: &str,
) {
    let (pool, repo, alice, bob) = resource;
    let (writer, reader, outsider) = tokens;
    let branch = "access-policy";
    let reference = format!("{base}/git/ref/heads/{branch}");
    call(
        router,
        "POST",
        &format!("{base}/git/refs"),
        writer,
        json!({"ref":format!("refs/heads/{branch}"),"sha":sha}),
        ("git/create-ref", 201),
    )
    .await;
    db::repositories::protection::upsert_branch_protection_rule(
        pool,
        db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: alice,
            repo_id: repo,
            pattern: branch,
            require_pull_request: false,
            restrict_pushes: true,
            push_allowances: vec![alice],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let path = format!("{base}/branches/{branch}/protection/restrictions");
    let users = format!("{path}/users");
    let policy = call(
        router,
        "GET",
        &path,
        reader,
        Value::Null,
        ("repos/get-access-restrictions", 200),
    )
    .await;
    assert_eq!(policy["users"][0]["login"], "alice");
    assert_eq!(policy["users"][0]["id"], alice);
    assert_eq!(policy["teams"], json!([]));
    assert_eq!(policy["apps"], json!([]));
    call(
        router,
        "GET",
        &users,
        reader,
        Value::Null,
        ("repos/get-users-with-access-to-protected-branch", 200),
    )
    .await;
    call(
        router,
        "POST",
        &users,
        reader,
        json!({"users":["bob"]}),
        ("repos/add-user-access-restrictions", 403),
    )
    .await;
    call(
        router,
        "POST",
        &users,
        outsider,
        json!({"users":["bob"]}),
        ("repos/add-user-access-restrictions", 404),
    )
    .await;
    call(
        router,
        "POST",
        &users,
        writer,
        json!({"users":["missing-user"]}),
        ("repos/add-user-access-restrictions", 422),
    )
    .await;
    call(
        router,
        "POST",
        &users,
        writer,
        json!({"users":["bob\0"]}),
        ("repos/add-user-access-restrictions", 422),
    )
    .await;
    call(
        router,
        "PUT",
        &users,
        writer,
        json!({"users":vec!["alice"; 101]}),
        ("repos/set-user-access-restrictions", 422),
    )
    .await;
    call(
        router,
        "POST",
        &users,
        writer,
        json!({"users":["bob"]}),
        ("repos/add-user-access-restrictions", 422),
    )
    .await;
    codeyard::backend::repositories::access::change_collaborator(
        pool,
        alice,
        repo,
        "bob",
        Some(db::models::AccessLevel::Write),
    )
    .await
    .unwrap();
    let added = call(
        router,
        "POST",
        &users,
        writer,
        json!({"users":["BOB","bob"]}),
        ("repos/add-user-access-restrictions", 200),
    )
    .await;
    assert_eq!(added.as_array().unwrap().len(), 2);
    assert!(added
        .as_array()
        .unwrap()
        .iter()
        .any(|user| user["id"] == bob));
    let removed = call(
        router,
        "DELETE",
        &users,
        writer,
        json!({"users":["bob"]}),
        ("repos/remove-user-access-restrictions", 200),
    )
    .await;
    assert_eq!(removed.as_array().unwrap().len(), 1);
    assert_eq!(removed[0]["id"], alice);

    let original = call(
        router,
        "GET",
        &format!("{base}/git/commits/{sha}"),
        reader,
        Value::Null,
        ("git/get-commit", 200),
    )
    .await;
    let candidate = call(router, "POST", &format!("{base}/git/commits"), writer,
        json!({"message":"A restricted branch update", "tree": original["tree"]["sha"], "parents":[sha]}), ("git/create-commit", 201)).await;
    let candidate = candidate["sha"].as_str().unwrap();
    let repository = db::repositories::records::get_repository_by_id(pool, repo)
        .await
        .unwrap()
        .unwrap();
    let store = repo_store(&repository, None);
    let empty = call(
        router,
        "PUT",
        &users,
        writer,
        json!([]),
        ("repos/set-user-access-restrictions", 200),
    )
    .await;
    assert_eq!(empty, json!([]));
    let denied = store
        .update_reference_as(
            pool,
            repo,
            alice,
            &format!("refs/heads/{branch}"),
            candidate,
            false,
        )
        .await
        .unwrap_err();
    assert!(denied.is_protected_branch(), "{denied:?}");
    assert_eq!(
        call(
            router,
            "GET",
            &reference,
            reader,
            Value::Null,
            ("git/get-ref", 200)
        )
        .await["object"]["sha"],
        sha
    );

    // Read/modify/write unions are serialized at the native authority boundary.
    tokio::join!(
        call(
            router,
            "POST",
            &users,
            writer,
            json!(["alice"]),
            ("repos/add-user-access-restrictions", 200)
        ),
        call(
            router,
            "POST",
            &users,
            writer,
            json!(["bob"]),
            ("repos/add-user-access-restrictions", 200)
        )
    );
    let union = call(
        router,
        "GET",
        &users,
        reader,
        Value::Null,
        ("repos/get-users-with-access-to-protected-branch", 200),
    )
    .await;
    assert_eq!(union.as_array().unwrap().len(), 2);
    for suffix in ["", "/users"] {
        for token in [reader, writer] {
            let (status, _, body) = request(
                router,
                "HEAD",
                &format!("{path}{suffix}"),
                Some(token),
                json!(["alice"]),
            )
            .await;
            assert_eq!(status.as_u16(), 200);
            assert_eq!(body, Value::Null);
        }
    }
    assert_eq!(
        call(
            router,
            "GET",
            &users,
            reader,
            Value::Null,
            ("repos/get-users-with-access-to-protected-branch", 200)
        )
        .await,
        union
    );
    // An allowance never retains repository write permission after that permission is revoked.
    codeyard::backend::repositories::access::change_collaborator(pool, alice, repo, "bob", None)
        .await
        .unwrap();
    let refused = db::repositories::protection::update_branch_protection_rule(
        pool,
        alice,
        repo,
        branch,
        db::repositories::protection::ProtectionUpdate::PushUsers(
            db::repositories::protection::ActorListUpdate::Add(vec![bob]),
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(
        refused.downcast_ref::<db::repositories::protection::ProtectionWriteRefusal>(),
        Some(&db::repositories::protection::ProtectionWriteRefusal::InvalidPushAllowances)
    );
    assert_eq!(
        call(
            router,
            "GET",
            &users,
            reader,
            Value::Null,
            ("repos/get-users-with-access-to-protected-branch", 200)
        )
        .await,
        union
    );
    assert!(store
        .update_reference_as(
            pool,
            repo,
            bob,
            &format!("refs/heads/{branch}"),
            candidate,
            false
        )
        .await
        .unwrap_err()
        .is_write_denied());
    let cleaned = call(
        router,
        "DELETE",
        &users,
        writer,
        json!(["bob"]),
        ("repos/remove-user-access-restrictions", 200),
    )
    .await;
    assert_eq!(cleaned.as_array().unwrap().len(), 1);
    assert_eq!(cleaned[0]["id"], alice);
    call(
        router,
        "DELETE",
        &path,
        reader,
        Value::Null,
        ("repos/delete-access-restrictions", 403),
    )
    .await;
    call(
        router,
        "DELETE",
        &path,
        writer,
        Value::Null,
        ("repos/delete-access-restrictions", 204),
    )
    .await;
    call(
        router,
        "GET",
        &path,
        reader,
        Value::Null,
        ("repos/get-access-restrictions", 404),
    )
    .await;
    call(
        router,
        "GET",
        &users,
        reader,
        Value::Null,
        ("repos/get-users-with-access-to-protected-branch", 404),
    )
    .await;
    // Disabling restrictions immediately restores the ordinary native write policy.
    store
        .update_reference_as(
            pool,
            repo,
            alice,
            &format!("refs/heads/{branch}"),
            candidate,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        call(
            router,
            "GET",
            &reference,
            reader,
            Value::Null,
            ("git/get-ref", 200)
        )
        .await["object"]["sha"],
        candidate
    );
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
        &format!("{base}/git/refs/heads/{branch}"),
        writer,
        Value::Null,
        ("git/delete-ref", 204),
    )
    .await;
}
