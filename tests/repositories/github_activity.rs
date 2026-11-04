//! Stars over GitHub REST, with both database engines and token scopes.
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn request(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let (status, _, body) = request_media(
        router,
        operation,
        method,
        path,
        token,
        "application/vnd.github+json",
    )
    .await;
    (status, body)
}

async fn request_media(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    accept: &str,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("accept", accept);
    if let Some(token) = token {
        request = request.header("authorization", token);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        let body = serde_json::from_slice(&bytes).unwrap();
        assert_github_schema(operation, status.as_u16(), &body);
        body
    };
    (status, headers, body)
}

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let mut repositories = Vec::new();
    for (name, private) in [("proj", false), ("secret", true)] {
        let repo = codeyard_app::repositories::lifecycle::provision(
            (f.pool(), None),
            root.path(),
            codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice_id),
            name,
            None,
            private,
            None,
        )
        .await
        .unwrap();
        repositories.push(repo);
    }
    let router = f.router(root.path()).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let mut credentials = Vec::new();
    for scope in ["read", "write"] {
        let token = tokens::create(
            f.pool(),
            bob_id,
            Some(&bob.session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let read = credentials[0].as_str();
    let write = credentials[1].as_str();
    let path = "/api/v3/user/starred/alice/proj";
    let (status, body) = request(
        &router,
        "activity/check-repo-is-starred-by-authenticated-user",
        "GET",
        path,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let (status, _) = request(
        &router,
        "activity/check-repo-is-starred-by-authenticated-user",
        "GET",
        path,
        Some(read),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = request(
        &router,
        "activity/star-repo-for-authenticated-user",
        "PUT",
        path,
        Some(read),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for _ in 0..2 {
        let (status, body) = request(
            &router,
            "activity/star-repo-for-authenticated-user",
            "PUT",
            path,
            Some(write),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    let (status, _) = request(
        &router,
        "activity/check-repo-is-starred-by-authenticated-user",
        "GET",
        path,
        Some(read),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = request(
        &router,
        "activity/get-stargazer-count-for-repo",
        "GET",
        "/api/v3/repos/alice/proj/stargazers/count",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"count": 1}));
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM stars WHERE repo_id=$1",
            &[repositories[0].id]
        )
        .await,
        1
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id=$1 AND kind='repository_starred'",
            &[alice_id]
        )
        .await,
        1
    );
    let (status, _, users) = request_media(
        &router,
        "activity/list-stargazers-for-repo",
        "GET",
        "/api/v3/repos/alice/proj/stargazers",
        None,
        "application/vnd.github.star+json",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{users}");
    assert_eq!(users[0]["user"]["id"], bob_id);
    assert!(users[0]["starred_at"].as_str().unwrap().contains('T'));
    let (_, users) = request(
        &router,
        "activity/list-stargazers-for-repo",
        "GET",
        "/api/v3/repos/alice/proj/stargazers",
        None,
    )
    .await;
    assert_eq!(users[0]["id"], bob_id);
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob_id,
        repositories[1].id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let (status, _) = request(
        &router,
        "activity/star-repo-for-authenticated-user",
        "PUT",
        "/api/v3/user/starred/alice/secret",
        Some(write),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, _, stars) = request_media(
        &router,
        "activity/list-repos-starred-by-user",
        "GET",
        "/api/v3/users/bob/starred",
        None,
        "application/vnd.github.star+json",
    )
    .await;
    assert_eq!(stars.as_array().unwrap().len(), 1);
    assert_eq!(stars[0]["repo"]["name"], "proj");
    let (status, headers, stars) = request_media(
        &router,
        "activity/list-repos-starred-by-authenticated-user",
        "GET",
        "/api/v3/user/starred?per_page=1&sort=created&direction=asc",
        Some(read),
        "application/vnd.github+json",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stars}");
    assert_eq!(stars[0]["name"], "proj");
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (_, stars) = request(
        &router,
        "activity/list-repos-starred-by-authenticated-user",
        "GET",
        "/api/v3/user/starred?per_page=1&page=2&direction=asc",
        Some(read),
    )
    .await;
    assert_eq!(stars[0]["name"], "secret");
    codeyard_db_repos::repositories::access::revoke_repo_access(
        f.pool(),
        bob_id,
        repositories[1].id,
    )
    .await
    .unwrap();
    let (_, stars) = request(
        &router,
        "activity/list-repos-starred-by-authenticated-user",
        "GET",
        "/api/v3/user/starred?sort=updated",
        Some(read),
    )
    .await;
    assert_eq!(stars.as_array().unwrap().len(), 1);
    assert_eq!(stars[0]["name"], "proj");
    let (status, _) = request(
        &router,
        "activity/unstar-repo-for-authenticated-user",
        "DELETE",
        path,
        Some(read),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for _ in 0..2 {
        let (status, _) = request(
            &router,
            "activity/unstar-repo-for-authenticated-user",
            "DELETE",
            path,
            Some(write),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let (_, body) = request(
        &router,
        "activity/get-stargazer-count-for-repo",
        "GET",
        "/api/v3/repos/alice/proj/stargazers/count",
        None,
    )
    .await;
    assert_eq!(body, json!({"count": 0}));
    for (method, operation) in [
        (
            "GET",
            "activity/check-repo-is-starred-by-authenticated-user",
        ),
        ("PUT", "activity/star-repo-for-authenticated-user"),
        ("DELETE", "activity/unstar-repo-for-authenticated-user"),
    ] {
        let (status, _) = request(
            &router,
            operation,
            method,
            "/api/v3/user/starred/alice/secret",
            Some(write),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (status, _) = request(
        &router,
        "activity/get-stargazer-count-for-repo",
        "GET",
        "/api/v3/repos/alice/secret/stargazers/count",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM stars WHERE repo_id=$1",
            &[repositories[0].id]
        )
        .await,
        0
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM stars WHERE repo_id=$1",
            &[repositories[1].id]
        )
        .await,
        1
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_github_activity_stars() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_activity_stars() {
    contract(true).await;
}
