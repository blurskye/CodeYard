//! Organization cache reports include real uploads, paging, and current tenant authority.
use super::{actions_objects::pat, results};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use crate::support::github_spec::assert_github_schema;
use crate::support::organizations::applied;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};

use super::NewOrganization;
use super::OrganizationProfile;
use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
use codeyard_db_repos::repositories::creation::RepositoryTarget;
use serde_json::Value;
use tower::ServiceExt;

async fn checked(
    router: &Router,
    path: &str,
    token: &str,
    expected: StatusCode,
) -> (HeaderMap, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        status,
        expected,
        "{path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    let body = serde_json::from_slice(&bytes).unwrap();
    if status == StatusCode::OK {
        assert_github_schema(
            if path.contains("usage-by-repository") {
                "actions/get-actions-cache-usage-by-repo-for-org"
            } else {
                "actions/get-actions-cache-usage-for-org"
            },
            200,
            &body,
        );
    } else {
        assert_github_schema("git/get-blob", 404, &body);
    }
    (headers, body)
}

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let site_admin = f.actor("site_admin").await;
    f.execute("UPDATE users SET is_admin=true WHERE id=$1", &[site_admin])
        .await;
    let org = applied(
        super::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("builders", OrganizationProfile::new(None, None).unwrap())
                .unwrap(),
        )
        .await
        .unwrap(),
    );
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'member')",
        &[org.id, member],
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (name, private) in [("one", true), ("two", false), ("empty", true)] {
        repos.push(
            codeyard_app::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                RepositoryCreationOwner {
                    actor_id: owner,
                    target: RepositoryTarget::Organization(org.id),
                },
                name,
                None,
                private,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let personal = codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "personal",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let owner_token = pat(&f, &router, owner, "read").await;
    let member_token = pat(&f, &router, member, "read").await;
    let admin_token = pat(&f, &router, site_admin, "read").await;
    let mut ids = Vec::new();
    for (repo, bytes) in [
        (&repos[0], b"first".as_slice()),
        (&repos[1], b"1234567".as_slice()),
        (&personal, b"personal cache".as_slice()),
    ] {
        let (_, token) = results::run(&f, repo.id, owner, "main").await;
        ids.push(results::cache(&router, &token.token, "deps", bytes).await);
    }
    let (_, token) = results::run(&f, repos[0].id, owner, "feature").await;
    let expired = results::cache(&router, &token.token, "expired", b"expired").await;
    f.execute(
        "UPDATE actions_objects SET expires_at=0 WHERE id=$1",
        &[expired],
    )
    .await;
    let base = "/api/v3/orgs/builders/actions/cache";
    let (_, body) = checked(
        &router,
        &format!("{base}/usage"),
        &owner_token,
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_active_caches_count"], 2);
    assert_eq!(body["total_active_caches_size_in_bytes"], 12);
    let (headers, body) = checked(
        &router,
        &format!("{base}/usage-by-repository?per_page=1"),
        &owner_token,
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_count"], 3);
    assert_eq!(
        body["repository_cache_usages"][0]["full_name"],
        "builders/one"
    );
    assert_eq!(
        body["repository_cache_usages"][0]["active_caches_size_in_bytes"],
        5
    );
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, body) = checked(
        &router,
        &format!("{base}/usage-by-repository?per_page=1&page=2"),
        &owner_token,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        body["repository_cache_usages"][0]["full_name"],
        "builders/two"
    );
    let (_, body) = checked(
        &router,
        &format!("{base}/usage-by-repository?per_page=1&page=3"),
        &owner_token,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        body["repository_cache_usages"][0]["full_name"],
        "builders/empty"
    );
    assert_eq!(body["repository_cache_usages"][0]["active_caches_count"], 0);
    for token in [&member_token, &admin_token] {
        for suffix in ["usage", "usage-by-repository"] {
            checked(
                &router,
                &format!("{base}/{suffix}"),
                token,
                StatusCode::FORBIDDEN,
            )
            .await;
        }
    }
    checked(
        &router,
        "/api/v3/orgs/missing/actions/cache/usage",
        &owner_token,
        StatusCode::NOT_FOUND,
    )
    .await;
    f.execute(
        "UPDATE org_memberships SET role='admin' WHERE org_id=$1 AND user_id=$2",
        &[org.id, member],
    )
    .await;
    checked(
        &router,
        &format!("{base}/usage"),
        &member_token,
        StatusCode::OK,
    )
    .await;
    f.execute(
        "UPDATE org_memberships SET role='member' WHERE org_id=$1 AND user_id=$2",
        &[org.id, member],
    )
    .await;
    checked(
        &router,
        &format!("{base}/usage"),
        &member_token,
        StatusCode::FORBIDDEN,
    )
    .await;
    f.execute(
        "UPDATE actions_objects SET expires_at=0 WHERE id=$1",
        &[ids[0]],
    )
    .await;
    let (_, body) = checked(
        &router,
        &format!("{base}/usage"),
        &owner_token,
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_active_caches_count"], 1);
    assert_eq!(body["total_active_caches_size_in_bytes"], 7);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_actions_org_usage() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_actions_org_usage() {
    contract(true).await;
}
