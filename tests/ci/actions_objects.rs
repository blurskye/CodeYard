//! Actions REST metadata and capability downloads operate on real runtime uploads on both databases.
use super::results;
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_app::ci;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tower::ServiceExt;

async fn request(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let path = url::Url::parse(path)
        .map(|url| {
            format!(
                "{}{}",
                url.path(),
                url.query().map(|q| format!("?{q}")).unwrap_or_default()
            )
        })
        .unwrap_or_else(|_| path.to_owned());
    let mut req = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

pub(super) async fn checked(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    expected: StatusCode,
) -> (HeaderMap, Value) {
    let (status, headers, bytes) = request(router, method, path, token).await;
    assert_eq!(
        status,
        expected,
        "{path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    if matches!(status, StatusCode::NO_CONTENT | StatusCode::FOUND) {
        assert!(bytes.is_empty());
        return (headers, Value::Null);
    }
    let body = serde_json::from_slice(&bytes).unwrap();
    if status.is_success() || status == StatusCode::GONE {
        assert_github_schema(operation, status.as_u16(), &body);
    } else {
        assert_github_schema("git/get-blob", 404, &body);
    }
    (headers, body)
}

pub(super) async fn artifact(
    router: &Router,
    run: &codeyard_db_core::models::CiRun,
    token: &str,
    name: &str,
    bytes: &[u8],
) -> i64 {
    let mut body = results::artifact_body(run, name);
    body["version"] = json!(4);
    let reply = results::rpc(
        router,
        token,
        results::ARTIFACT,
        "CreateArtifact",
        body.clone(),
        StatusCode::OK,
    )
    .await;
    results::put(
        router,
        reply["signedUploadUrl"].as_str().unwrap(),
        bytes,
        StatusCode::CREATED,
    )
    .await;
    body["size"] = json!(bytes.len());
    results::rpc(
        router,
        token,
        results::ARTIFACT,
        "FinalizeArtifact",
        body,
        StatusCode::OK,
    )
    .await["artifactId"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

pub(super) async fn pat(f: &Database, router: &Router, actor: i64, scope: &str) -> String {
    let client = Client::login(router, f.pool(), actor).await;
    tokens::create(
        f.pool(),
        actor,
        Some(&client.session),
        TokenInput {
            name: "Actions",
            scope,
            expires_at: None,
        },
    )
    .await
    .unwrap()
    .value
}

/// Issues the job credentials a runner gets for `jobs` of ci.yml, which it must hold before it
/// reports their progress.
pub(super) async fn job_credentials(
    f: &Database,
    run: &codeyard_db_core::models::CiRun,
    jobs: &[&str],
) {
    let jobs: String = jobs
        .iter()
        .map(|job| {
            format!("  {job}:\n    runs-on: ubuntu-latest\n    steps: [{{run: 'echo ok'}}]\n")
        })
        .collect();
    let definitions = BTreeMap::from([(
        ".github/workflows/ci.yml".to_owned(),
        format!("on: workflow_dispatch\njobs:\n{jobs}").into_bytes(),
    )]);
    ci::tokens::issue(f.pool(), run, &definitions, false)
        .await
        .unwrap();
}

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for name in ["project", "separate"] {
        repos.push(
            codeyard_app::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
                name,
                None,
                true,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let repo = &repos[0];
    let router = f.router(root.path()).await;
    let writer = pat(&f, &router, alice, "write").await;
    let reader = pat(&f, &router, alice, "read").await;
    let outsider = pat(&f, &router, bob, "write").await;
    let (main, main_token) = results::run(&f, repo.id, alice, "main").await;
    let (feature, feature_token) = results::run(&f, repo.id, alice, "feature").await;
    let cache1 = results::cache(&router, &main_token.token, "deps-v1", b"one").await;
    let cache2 = results::cache(&router, &main_token.token, "deps-v2", b"three").await;
    let cache3 = results::cache(&router, &feature_token.token, "deps-v1", b"four").await;
    let mut zip = vec![0; 22];
    zip[..4].copy_from_slice(b"PK\x05\x06");
    let first = artifact(&router, &main, &main_token.token, "dist", &zip).await;
    let second = artifact(&router, &main, &main_token.token, "dist-debug", &zip).await;
    let third = artifact(&router, &feature, &feature_token.token, "dist", &zip).await;
    let base = "/api/v3/repos/alice/project/actions";
    let (headers, body) = checked(
        &router,
        "actions/list-artifacts-for-repo",
        "GET",
        &format!("{base}/artifacts?name=dist&per_page=1"),
        Some(&reader),
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_count"], 2);
    assert_eq!(body["artifacts"][0]["id"], third);
    let link = headers["link"].to_str().unwrap();
    assert!(
        link.contains("page=2") && link.contains("name=dist"),
        "{link}"
    );
    let (_, body) = checked(
        &router,
        "actions/list-artifacts-for-repo",
        "GET",
        &format!("{base}/artifacts?name=dist&per_page=1&page=2"),
        Some(&reader),
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["artifacts"][0]["id"], first);
    let (_, body) = checked(
        &router,
        "actions/list-workflow-run-artifacts",
        "GET",
        &format!("{base}/runs/{}/artifacts", main.id),
        Some(&reader),
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_count"], 2);
    let (_, body) = checked(
        &router,
        "actions/get-artifact",
        "GET",
        &format!("{base}/artifacts/{first}"),
        Some(&reader),
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["workflow_run"]["id"], main.id);
    assert_eq!(body["size_in_bytes"], 22);
    assert_eq!(body["expired"], false);
    for suffix in [
        "artifacts".to_owned(),
        format!("artifacts/{first}"),
        "caches".to_owned(),
        "cache/usage".to_owned(),
    ] {
        for token in [None, Some(outsider.as_str())] {
            checked(
                &router,
                "",
                "GET",
                &format!("{base}/{suffix}"),
                token,
                StatusCode::NOT_FOUND,
            )
            .await;
        }
    }
    checked(
        &router,
        "",
        "GET",
        &format!("/api/v3/repos/alice/separate/actions/artifacts/{first}"),
        Some(&reader),
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "",
        "GET",
        &format!(
            "/api/v3/repos/alice/separate/actions/runs/{}/artifacts",
            main.id
        ),
        Some(&reader),
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "",
        "DELETE",
        &format!("{base}/artifacts/{first}"),
        Some(&reader),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "",
        "GET",
        &format!("{base}/artifacts/{first}/tar"),
        Some(&reader),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let (redirect, _) = checked(
        &router,
        "actions/download-artifact",
        "GET",
        &format!("{base}/artifacts/{first}/zip"),
        Some(&reader),
        StatusCode::FOUND,
    )
    .await;
    let signed = redirect["location"].to_str().unwrap();
    let (status, headers, bytes) = request(&router, "GET", signed, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/zip");
    assert_eq!(bytes, zip);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM actions_blob_grants WHERE object_id=$1 AND NOT write AND expires_at>$2 AND expires_at<=$2+60",&[first,chrono::Utc::now().timestamp()]).await,1);
    checked(
        &router,
        "actions/delete-artifact",
        "DELETE",
        &format!("{base}/artifacts/{first}"),
        Some(&writer),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        request(&router, "GET", signed, None).await.0,
        StatusCode::FORBIDDEN
    );
    checked(
        &router,
        "",
        "GET",
        &format!("{base}/artifacts/{first}"),
        Some(&reader),
        StatusCode::NOT_FOUND,
    )
    .await;
    f.execute(
        "UPDATE actions_objects SET expires_at=0 WHERE id=$1",
        &[second],
    )
    .await;
    let (_, body) = checked(
        &router,
        "actions/get-artifact",
        "GET",
        &format!("{base}/artifacts/{second}"),
        Some(&reader),
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["expired"], true);
    checked(
        &router,
        "actions/download-artifact",
        "GET",
        &format!("{base}/artifacts/{second}/zip"),
        Some(&reader),
        StatusCode::GONE,
    )
    .await;
    checked(
        &router,
        "actions/delete-artifact",
        "DELETE",
        &format!("{base}/artifacts/{second}"),
        Some(&writer),
        StatusCode::NO_CONTENT,
    )
    .await;

    let (_, body) = checked(
        &router,
        "actions/get-actions-cache-usage",
        "GET",
        &format!("{base}/cache/usage"),
        Some(&reader),
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["active_caches_count"], 3);
    assert_eq!(body["active_caches_size_in_bytes"], 12);
    let (headers, body) = checked(
        &router,
        "actions/get-actions-cache-list",
        "GET",
        &format!("{base}/caches?key=deps-&sort=size_in_bytes&direction=asc&per_page=1"),
        Some(&reader),
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_count"], 3);
    assert_eq!(body["actions_caches"][0]["id"], cache1);
    assert!(headers.contains_key("link"));
    let (_, body) = checked(
        &router,
        "actions/get-actions-cache-list",
        "GET",
        &format!("{base}/caches?ref=refs/heads/main"),
        Some(&reader),
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_count"], 2);
    assert_eq!(body["actions_caches"][0]["ref"], "refs/heads/main");
    checked(
        &router,
        "",
        "DELETE",
        &format!("{base}/caches/{cache1}"),
        Some(&reader),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "",
        "DELETE",
        &format!("{base}/caches"),
        Some(&writer),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let (_, body) = checked(
        &router,
        "actions/delete-actions-cache-by-key",
        "DELETE",
        &format!("{base}/caches?key=deps-v1&ref=refs/heads/main"),
        Some(&writer),
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_count"], 1);
    assert_eq!(body["actions_caches"][0]["id"], cache1);
    let job_tokens=ci::tokens::issue(f.pool(),&main,&BTreeMap::from([(".github/workflows/ci.yml".to_owned(),b"jobs:\n  reader:\n    permissions:\n      actions: read\n  writer:\n    permissions:\n      actions: write\n".to_vec())]),false).await.unwrap();
    let jobs = &job_tokens.workflows[".github/workflows/ci.yml"];
    checked(
        &router,
        "actions/get-actions-cache-list",
        "GET",
        &format!("{base}/caches"),
        Some(&jobs["reader"]),
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        "",
        "DELETE",
        &format!("{base}/caches/{cache3}"),
        Some(&jobs["reader"]),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "actions/delete-actions-cache-by-id",
        "DELETE",
        &format!("{base}/caches/{cache3}"),
        Some(&jobs["writer"]),
        StatusCode::NO_CONTENT,
    )
    .await;
    let (_, body) = checked(
        &router,
        "actions/get-actions-cache-usage",
        "GET",
        &format!("{base}/cache/usage"),
        Some(&reader),
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["active_caches_count"], 1);
    assert_eq!(body["active_caches_size_in_bytes"], 5);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_objects WHERE id=$1",
            &[cache2]
        )
        .await,
        1
    );
    assert!(
        f.scalar("SELECT COUNT(*) FROM pending_object_deletions", &[])
            .await
            >= 4
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_actions_objects() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_actions_objects() {
    contract(true).await;
}
