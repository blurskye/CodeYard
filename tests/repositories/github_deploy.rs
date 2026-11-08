//! Deployment configuration through the real REST router on both engines.
mod deployments;
mod meta;
mod oidc;
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};

use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn checked(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    input: Value,
    expected: u16,
) -> (HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", token);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(input.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status.as_u16(),
        expected,
        "{operation}: {}",
        String::from_utf8_lossy(&bytes)
    );
    if status == StatusCode::NO_CONTENT || status.is_redirection() || method == "HEAD" {
        assert!(bytes.is_empty());
        return (headers, Value::Null);
    }
    let body = serde_json::from_slice(&bytes).unwrap();
    assert_github_schema(operation, status.as_u16(), &body);
    (headers, body)
}

async fn environments_contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let repo = codeyard_git_storage::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
        "private",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let mut credentials = Vec::new();
    for (actor, scope) in [(alice, "write"), (alice, "read"), (bob, "write")] {
        let session = Client::login(&router, f.pool(), actor).await;
        let issued = tokens::create(
            f.pool(),
            actor,
            Some(&session.session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", issued.value));
    }
    let (writer, reader, outsider) = (&credentials[0], &credentials[1], &credentials[2]);
    let base = "/api/v3/repos/alice/private/environments";
    let prod = format!("{base}/Production");
    let input = json!({"wait_timer":5,"prevent_self_review":true,"reviewers":[{"type":"User","id":alice}],"deployment_branch_policy":{"protected_branches":false,"custom_branch_policies":true}});
    for (token, expected) in [
        (Some(reader.as_str()), 403),
        (Some(outsider.as_str()), 404),
        (None, 401),
    ] {
        checked(
            &router,
            "repos/create-or-update-environment",
            "PUT",
            &prod,
            token,
            input.clone(),
            expected,
        )
        .await;
    }
    let (_, env) = checked(
        &router,
        "repos/create-or-update-environment",
        "PUT",
        &prod,
        Some(writer),
        input,
        200,
    )
    .await;
    assert_eq!(env["name"], "Production");
    assert_eq!(env["protection_rules"][0]["wait_timer"], 5);
    assert_eq!(
        env["protection_rules"][1]["reviewers"][0]["reviewer"]["id"],
        alice
    );
    assert_eq!(env["protection_rules"][1]["prevent_self_review"], true);
    let (_, read) = checked(
        &router,
        "repos/get-environment",
        "GET",
        &format!("{base}/production"),
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(env, read);
    for input in [
        json!({"wait_timer":43201}),
        json!({"wait_timer":-1}),
        json!({"reviewers":[{"type":"User","id":bob}]}),
        json!({"reviewers":[{"type":"User","id":999999}]}),
        json!({"reviewers":[{"type":"Robot","id":alice}]}),
        json!({"deployment_branch_policy":{"protected_branches":true,"custom_branch_policies":true}}),
        json!({"deployment_branch_policy":{"protected_branches":false,"custom_branch_policies":false}}),
    ] {
        checked(
            &router,
            "repos/create-or-update-environment",
            "PUT",
            &prod,
            Some(writer),
            input,
            422,
        )
        .await;
    }
    checked(
        &router,
        "repos/create-or-update-environment",
        "PUT",
        &format!("{base}/staging%2Fpreview"),
        Some(writer),
        json!({}),
        200,
    )
    .await;
    let (headers, page) = checked(
        &router,
        "repos/get-all-environments",
        "GET",
        &format!("{base}?per_page=1"),
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(page["total_count"], 2);
    assert_eq!(page["environments"].as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    checked(
        &router,
        "repos/get-environment",
        "GET",
        &prod,
        Some(outsider),
        Value::Null,
        404,
    )
    .await;
    checked(
        &router,
        "repos/get-all-environments",
        "GET",
        base,
        Some(outsider),
        Value::Null,
        404,
    )
    .await;
    checked(
        &router,
        "repos/get-environment",
        "GET",
        &format!("{base}/missing"),
        Some(reader),
        Value::Null,
        404,
    )
    .await;
    let policies = format!("{prod}/deployment-branch-policies");
    let (_, branch) = checked(
        &router,
        "repos/create-deployment-branch-policy",
        "POST",
        &policies,
        Some(writer),
        json!({"name":"release/*"}),
        200,
    )
    .await;
    let branch_path = format!("{policies}/{}", branch["id"]);
    let (headers, _) = checked(
        &router,
        "repos/create-deployment-branch-policy",
        "POST",
        &policies,
        Some(writer),
        json!({"name":"release/*"}),
        303,
    )
    .await;
    assert!(headers["location"]
        .to_str()
        .unwrap()
        .ends_with(&branch_path));
    let (_, tag) = checked(
        &router,
        "repos/create-deployment-branch-policy",
        "POST",
        &policies,
        Some(writer),
        json!({"name":"release/*","type":"tag"}),
        200,
    )
    .await;
    assert_eq!(tag["type"], "tag");
    let (headers, rows) = checked(
        &router,
        "repos/list-deployment-branch-policies",
        "GET",
        &format!("{policies}?per_page=1"),
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(rows["total_count"], 2);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (_, got) = checked(
        &router,
        "repos/get-deployment-branch-policy",
        "GET",
        &branch_path,
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(got, branch);
    let (_, updated) = checked(
        &router,
        "repos/update-deployment-branch-policy",
        "PUT",
        &branch_path,
        Some(writer),
        json!({"name":"main"}),
        200,
    )
    .await;
    assert_eq!(updated["type"], "branch");
    assert_eq!(updated["name"], "main");
    checked(
        &router,
        "repos/create-deployment-branch-policy",
        "POST",
        &format!("{base}/staging%2Fpreview/deployment-branch-policies"),
        Some(writer),
        json!({"name":"main"}),
        404,
    )
    .await;
    checked(
        &router,
        "repos/create-deployment-branch-policy",
        "POST",
        &policies,
        Some(reader),
        json!({"name":"no"}),
        403,
    )
    .await;
    checked(
        &router,
        "repos/create-deployment-branch-policy",
        "POST",
        &policies,
        Some(writer),
        json!({"name":"","type":"branch"}),
        422,
    )
    .await;
    checked(
        &router,
        "repos/get-deployment-branch-policy",
        "GET",
        &format!("{policies}/9999999"),
        Some(reader),
        Value::Null,
        404,
    )
    .await;
    checked(
        &router,
        "repos/delete-deployment-branch-policy",
        "DELETE",
        &branch_path,
        Some(writer),
        Value::Null,
        204,
    )
    .await;
    checked(
        &router,
        "repos/get-deployment-branch-policy",
        "GET",
        &branch_path,
        Some(reader),
        Value::Null,
        404,
    )
    .await;
    checked(
        &router,
        "repos/delete-an-environment",
        "DELETE",
        &prod,
        Some(reader),
        Value::Null,
        403,
    )
    .await;
    checked(
        &router,
        "repos/delete-an-environment",
        "DELETE",
        &prod,
        Some(writer),
        Value::Null,
        204,
    )
    .await;
    checked(
        &router,
        "repos/get-environment",
        "GET",
        &prod,
        Some(reader),
        Value::Null,
        404,
    )
    .await;
    assert!(codeyard_db_actions::environments::branches(
        f.pool(),
        env["id"].as_i64().unwrap(),
        0,
        10
    )
    .await
    .unwrap()
    .is_empty());
    assert!(
        codeyard_db_actions::environments::get(f.pool(), repo.id, "production")
            .await
            .unwrap()
            .is_none()
    );
    checked(
        &router,
        "repos/delete-an-environment",
        "DELETE",
        &prod,
        Some(writer),
        Value::Null,
        204,
    )
    .await;
}

#[tokio::test]
async fn sqlite_github_environments() {
    environments_contract(false).await;
}
#[tokio::test]
async fn postgres_github_environments() {
    environments_contract(true).await;
}
