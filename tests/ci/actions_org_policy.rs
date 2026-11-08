//! Organization cache ceilings constrain existing objects and stale runtime preflights.
use super::{
    actions_objects::{artifact, pat},
    results,
};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::visible_text;
use crate::support::http_client::Client;
use crate::support::organizations::applied;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use codeyard_db_orgs::organizations::profiles::NewOrganization;
use codeyard_db_orgs::organizations::profiles::OrganizationProfile;

use codeyard_database::queries::actions::CacheLimit;
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_db_actions::action_objects as objects;
use codeyard_db_actions::action_objects::NewObject;
use codeyard_db_actions::actions::RepoActionsSettings;
use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
use codeyard_db_repos::repositories::creation::RepositoryTarget;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn checked(
    router: &Router,
    name: &str,
    kind: &str,
    method: &str,
    token: &str,
    body: Value,
    expected: StatusCode,
) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(format!(
                    "/api/v3/organizations/{name}/actions/cache/{kind}-limit"
                ))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(status, expected, "{}", String::from_utf8_lossy(&bytes));
    if status == StatusCode::NO_CONTENT {
        assert!(bytes.is_empty());
        return Value::Null;
    }
    let value = serde_json::from_slice(&bytes).unwrap();
    assert_github_schema(
        &format!(
            "actions/{}-actions-cache-{kind}-limit-for-organization",
            if method == "GET" { "get" } else { "set" }
        ),
        status.as_u16(),
        &value,
    );
    value
}

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let site_admin = f.actor("site_admin").await;
    f.execute("UPDATE users SET is_admin=true WHERE id=$1", &[site_admin])
        .await;
    let mut organizations = Vec::new();
    for (name, owner) in [("builders", alice), ("outsiders", bob)] {
        organizations.push(applied(
            codeyard_db_orgs::organizations::profiles::create_organization(
                f.pool(),
                owner,
                &NewOrganization::new(name, OrganizationProfile::new(None, None).unwrap()).unwrap(),
            )
            .await
            .unwrap(),
        ));
    }
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'member')",
        &[organizations[0].id, bob],
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (org, actor) in [(&organizations[0], alice), (&organizations[1], bob)] {
        repos.push(
            codeyard_git_storage::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                RepositoryCreationOwner {
                    actor_id: actor,
                    target: RepositoryTarget::Organization(org.id),
                },
                "project",
                None,
                true,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let personal = codeyard_git_storage::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner::personal(alice),
        "personal",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let repo = &repos[0];
    let router = f.router(root.path()).await;
    let writer = pat(&f, &router, alice, "write").await;
    let reader = pat(&f, &router, alice, "read").await;
    let member = pat(&f, &router, bob, "write").await;
    let administrator = pat(&f, &router, site_admin, "write").await;
    codeyard_db_actions::actions::save_repo_actions_settings(
        f.pool(),
        alice,
        repo.id,
        &RepoActionsSettings {
            cache_size_gib: Some(5),
            run_timeout_minutes: Some(17),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        checked(
            &router,
            "builders",
            "storage",
            "GET",
            &reader,
            Value::Null,
            StatusCode::OK
        )
        .await["max_cache_size_gb"],
        10
    );
    assert_eq!(
        checked(
            &router,
            "builders",
            "retention",
            "GET",
            &reader,
            Value::Null,
            StatusCode::OK
        )
        .await["max_cache_retention_days"],
        7
    );
    let (run, runtime) = results::run(&f, repo.id, alice, "main").await;
    let old = results::cache(&router, &runtime.token, "old", b"old").await;
    let recent = results::cache(&router, &runtime.token, "recent", b"cache").await;
    let mut zip = vec![0; 22];
    zip[..4].copy_from_slice(b"PK\x05\x06");
    let artifact_id = artifact(&router, &run, &runtime.token, "dist", &zip).await;
    tokio::join!(
        checked(
            &router,
            "builders",
            "storage",
            "PUT",
            &writer,
            json!({"max_cache_size_gb":2}),
            StatusCode::NO_CONTENT
        ),
        checked(
            &router,
            "builders",
            "retention",
            "PUT",
            &writer,
            json!({"max_cache_retention_days":3}),
            StatusCode::NO_CONTENT
        )
    );
    assert_eq!(
        checked(
            &router,
            "builders",
            "storage",
            "GET",
            &reader,
            Value::Null,
            StatusCode::OK
        )
        .await["max_cache_size_gb"],
        2
    );
    assert_eq!(
        checked(
            &router,
            "builders",
            "retention",
            "GET",
            &reader,
            Value::Null,
            StatusCode::OK
        )
        .await["max_cache_retention_days"],
        3
    );
    let own = codeyard_db_actions::actions::get_repo_actions_settings(f.pool(), repo.id)
        .await
        .unwrap();
    assert_eq!(own.cache_size_gib, Some(5));
    assert_eq!(own.run_timeout_minutes, Some(17));
    assert_eq!(own.org_cache_size_gib, Some(2));
    assert_eq!(own.org_cache_retention_days, Some(3));
    let site = codeyard_db_actions::actions::get_actions_settings(f.pool())
        .await
        .unwrap();
    assert_eq!(site.cache_size_bytes(&own), 2 * 1024 * 1024 * 1024);
    for repo in [&repos[1], &personal] {
        let settings = codeyard_db_actions::actions::get_repo_actions_settings(f.pool(), repo.id)
            .await
            .unwrap();
        assert_eq!(site.cache_size_bytes(&settings), 10 * 1024 * 1024 * 1024);
        assert_eq!(site.cache_expiry_days(&settings), 7);
    }
    assert_eq!(
        f.scalar(
            "SELECT expires_at-accessed_at FROM actions_objects WHERE id=$1",
            &[recent]
        )
        .await,
        3 * 86400
    );
    assert!(
        f.scalar(
            "SELECT expires_at-accessed_at FROM actions_objects WHERE id=$1",
            &[artifact_id]
        )
        .await
            >= 89 * 86400
    );
    let credentials = objects::authenticate(
        f.pool(),
        &codeyard_support::token_hash::token_hash(&runtime.token),
    )
    .await
    .unwrap()
    .unwrap();
    let now = chrono::Utc::now().timestamp();
    let stale = objects::create_object(
        f.pool(),
        NewObject {
            run: &credentials,
            kind: "cache",
            name: "stale-policy",
            version: "v1",
            expires_at: now + 7 * 86400,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(stale.expires_at <= now + 3 * 86400);
    for token in [&member, &administrator] {
        for kind in ["storage", "retention"] {
            checked(
                &router,
                "builders",
                kind,
                "GET",
                token,
                Value::Null,
                StatusCode::FORBIDDEN,
            )
            .await;
            checked(
                &router,
                "builders",
                kind,
                "PUT",
                token,
                json!({"max_cache_size_gb":1,"max_cache_retention_days":1}),
                StatusCode::FORBIDDEN,
            )
            .await;
        }
    }
    checked(
        &router,
        "builders",
        "storage",
        "PUT",
        &reader,
        json!({"max_cache_size_gb":1}),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "missing",
        "storage",
        "GET",
        &reader,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "missing",
        "storage",
        "PUT",
        &writer,
        json!({"max_cache_size_gb":1}),
        StatusCode::NOT_FOUND,
    )
    .await;
    for value in [0, -1, 11] {
        checked(
            &router,
            "builders",
            "storage",
            "PUT",
            &writer,
            json!({"max_cache_size_gb":value}),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    checked(
        &router,
        "builders",
        "retention",
        "PUT",
        &writer,
        json!({"max_cache_retention_days":8}),
        StatusCode::BAD_REQUEST,
    )
    .await;
    checked(
        &router,
        "builders",
        "retention",
        "PUT",
        &writer,
        json!({}),
        StatusCode::BAD_REQUEST,
    )
    .await;
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v3/repos/builders/project/actions/cache/storage-limit")
                .header("authorization", format!("Bearer {writer}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"max_cache_size_gb":3}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let client = Client::login(&router, f.pool(), alice).await;
    let path = "/builders/project/settings?tab=actions";
    let html = visible_text(&client.page_ok(path).await);
    assert!(
        html.contains("Current effective cache limits: 2 GiB and 3 days."),
        "{html}"
    );
    let (status, page) = client.model(path).await;
    assert_eq!(status, StatusCode::OK);
    let model = serde_json::to_string(&page).unwrap();
    assert!(model.contains("\"effective_cache_size_gib\":2"));
    assert!(model.contains("\"effective_cache_retention_days\":3"));
    // Simulate a large old entry without transferring a GiB in the test; the real upload
    // and manifest already exercise the runtime protocol above.
    f.execute(
        "UPDATE actions_objects SET size=1073741824,accessed_at=accessed_at-100 WHERE id=$1",
        &[old],
    )
    .await;
    checked(
        &router,
        "builders",
        "storage",
        "PUT",
        &writer,
        json!({"max_cache_size_gb":1}),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM actions_objects WHERE id=$1", &[old])
            .await,
        0
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_objects WHERE id=$1",
            &[recent]
        )
        .await,
        1
    );
    f.execute(
        "UPDATE org_memberships SET role='admin' WHERE org_id=$1 AND user_id=$2",
        &[organizations[0].id, bob],
    )
    .await;
    checked(
        &router,
        "builders",
        "storage",
        "GET",
        &member,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    f.execute(
        "DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2",
        &[organizations[0].id, bob],
    )
    .await;
    assert!(matches!(
        codeyard_db_actions::actions::set_org_cache_limit(
            f.pool(),
            bob,
            "builders",
            CacheLimit::Storage,
            2
        )
        .await
        .unwrap(),
        OrganizationOutcome::Denied
    ));
    assert_eq!(f.scalar("SELECT COUNT(*) FROM audit_log WHERE action='organization.actions_cache_policy' AND target_id=$1",&[organizations[0].id]).await,3);
    f.finish().await;
}
#[tokio::test]
async fn sqlite_actions_org_policy() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_actions_org_policy() {
    contract(true).await;
}
