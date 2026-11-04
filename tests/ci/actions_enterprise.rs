//! Absent tenancy and provider identities must never become fabricated data.
use crate::support::{database::Database, github_spec::assert_github_schema};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut fixture = Database::new(pg).await;
    fixture.complete_setup().await;
    let root = tempfile::tempdir().unwrap();
    let router = fixture.router(root.path()).await;
    for limit in ["retention", "storage"] {
        for (method, verb) in [("GET", "get"), ("PUT", "set")] {
            let operation = format!("actions/{verb}-actions-cache-{limit}-limit-for-enterprise");
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!(
                            "/api/v3/enterprises/example/actions/cache/{limit}-limit"
                        ))
                        .header("content-type", "application/json")
                        .body(Body::from(json!({"value":1}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{operation}");
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            assert_github_schema(&operation, 404, &body);
            assert_eq!(body["status"], "404");
        }
    }
    let actor = fixture.actor("alice").await;
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (fixture.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(actor),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        crate::support::production_ci::commit(&git, "refs/heads/main", None, "Build", None);
    }
    let api = super::actions_workflows::router(&fixture, root.path(), actor).await;
    let token = super::actions_objects::pat(&fixture, &api, actor, "write").await;
    let (_, run) = super::actions_workflows::checked(
        &api,
        "actions/create-workflow-dispatch",
        "POST",
        "/api/v3/repos/alice/project/actions/workflows/ci.yml/dispatches",
        &token,
        json!({"ref":"main","return_run_details":true}),
        StatusCode::OK,
    )
    .await;
    // Only the gate's own GitHub App decides it; an owner token is refused.
    let (_, refused) = super::actions_workflows::checked(
        &api,
        "actions/review-custom-gates-for-run",
        "POST",
        &format!(
            "/api/v3/repos/alice/project/actions/runs/{}/deployment_protection_rule",
            run["workflow_run_id"]
        ),
        &token,
        json!({"environment_name":"production","state":"approved","comment":"An owner PAT is not a provider identity"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    assert_eq!(refused["message"], "Resource not accessible by integration");
    assert_eq!(
        fixture
            .scalar("SELECT COUNT(*) FROM deployment_statuses", &[])
            .await,
        0
    );
    assert_eq!(
        fixture
            .scalar("SELECT COUNT(*) FROM repository_deployments", &[])
            .await,
        0
    );
    assert_eq!(
        fixture
            .scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1", &[repo.id])
            .await,
        1
    );
    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_enterprise_cache_limits_are_absent() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_enterprise_cache_limits_are_absent() {
    contract(true).await;
}
