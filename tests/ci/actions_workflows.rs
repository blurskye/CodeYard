//! GitHub workflow discovery, immutable dispatch, and disabling at durable admission on both databases.
use super::actions_objects::pat;
use crate::support::{database::Database, fake_webui_bundle, github_spec::assert_github_schema};
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db,
    repositories::store::{repo_store, FileWrite},
    storage::ObjectStore,
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, path::Path, sync::Arc};
use tower::ServiceExt;

pub(super) async fn router(f: &Database, root: &Path, actor: i64) -> Router {
    create_router(AppState::new(
        f.pool().clone(),
        root.to_owned(),
        Arc::new(ObjectStore::open(root, None).await.unwrap()),
        None,
        CiConfig {
            trusted_actor_ids: BTreeSet::from([actor]),
            ..Default::default()
        },
        None,
    ))
}

pub(super) async fn raw(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(if body.is_null() {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap(),
        )
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
    token: &str,
    body: Value,
    expected: StatusCode,
) -> (HeaderMap, Value) {
    let (status, headers, bytes) = raw(router, method, path, token, body).await;
    assert_eq!(
        status,
        expected,
        "{path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    if status == StatusCode::NO_CONTENT {
        assert!(bytes.is_empty());
        return (headers, Value::Null);
    }
    let body = serde_json::from_slice(&bytes).unwrap();
    if status.is_success() {
        assert_github_schema(operation, status.as_u16(), &body);
    } else {
        assert_github_schema("git/get-blob", 404, &body);
    }
    (headers, body)
}

const EXTRA:&str="name: 'Build \"<tag>\" & docs'\non:\n  workflow_dispatch:\n    inputs:\n      text:\n        type: string\n        default: hello\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo test\n";

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let collaborator = f.actor("collaborator").await;
    let root = tempfile::tempdir().unwrap();
    let repos_root = root.path().join("repos");
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        &repos_root,
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    f.execute(
        "INSERT INTO repo_access(user_id,repo_id,access_level) VALUES($1,$2,3)",
        &[collaborator, repo.id],
    )
    .await;
    let api = router(&f, &repos_root, alice).await;
    let writer = pat(&f, &api, alice, "write").await;
    let reader = pat(&f, &api, alice, "read").await;
    let outsider = pat(&f, &api, bob, "write").await;
    let contributor = pat(&f, &api, collaborator, "write").await;
    let base = "/api/v3/repos/alice/project/actions/workflows";
    let (_, body) = checked(
        &api,
        "actions/list-repo-workflows",
        "GET",
        base,
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_count"], 0);
    let sha = {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        crate::support::production_ci::commit(&git, "refs/heads/main", None, "First", None)
    };
    let store = repo_store(&repo, None);
    store
        .save_file_as(
            f.pool(),
            repo.id,
            alice,
            &FileWrite {
                branch: "main".to_owned(),
                path: ".github/workflows/extra.yml".to_owned(),
                message: "Add a workflow".to_owned(),
                expected_revision: sha,
                creating: true,
                deleting: false,
            },
            EXTRA.as_bytes(),
            "Alice",
            "alice@example.invalid",
        )
        .await
        .unwrap();
    let (headers, body) = checked(
        &api,
        "actions/list-repo-workflows",
        "GET",
        &format!("{base}?per_page=1"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_count"], 2);
    assert_eq!(body["workflows"].as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, body) = checked(
        &api,
        "actions/get-workflow",
        "GET",
        &format!("{base}/extra.yml"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let id = body["id"].as_i64().unwrap();
    assert_eq!(body["name"], "Build \"<tag>\" & docs");
    assert_eq!(body["path"], ".github/workflows/extra.yml");
    assert_eq!(body["state"], "active");
    let (_, numeric) = checked(
        &api,
        "actions/get-workflow",
        "GET",
        &format!("{base}/{id}"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(body, numeric);
    checked(
        &api,
        "",
        "GET",
        base,
        &outsider,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &api,
        "",
        "GET",
        &format!("{base}/missing.yml"),
        &reader,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &api,
        "",
        "PUT",
        &format!("{base}/{id}/disable"),
        &reader,
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &api,
        "actions/disable-workflow",
        "PUT",
        &format!("{base}/{id}/disable"),
        &writer,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let restarted = router(&f, &repos_root, alice).await;
    let (_, body) = checked(
        &restarted,
        "actions/get-workflow",
        "GET",
        &format!("{base}/{id}"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["state"], "disabled_manually");
    let badge = body["badge_url"].as_str().unwrap();
    let badge = url::Url::parse(badge).unwrap();
    let (status, headers, svg) = raw(&api, "GET", badge.path(), &reader, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/svg+xml; charset=utf-8");
    let svg = String::from_utf8(svg).unwrap();
    assert!(svg.contains("disabled"));
    assert!(svg.contains("&quot;&lt;tag&gt;&quot; &amp; docs"));
    assert!(!svg.contains("<tag>"));
    checked(
        &api,
        "",
        "POST",
        &format!("{base}/{id}/dispatches"),
        &writer,
        json!({"ref":"main"}),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1", &[repo.id])
            .await,
        0
    );
    let admitted_sha = store.commit("refs/heads/main").await.unwrap().sha;
    let blob = store
        .ci_workflow_file(&admitted_sha, ".github/workflows/extra.yml")
        .await
        .unwrap()
        .blob_id;
    let refused = db::ci::enqueue_ci_run(
        f.pool(),
        db::ci::NewCiRun {
            repo_id: repo.id,
            triggered_by_id: alice,
            pipeline: "ci",
            event: "workflow_dispatch",
            pull_request_id: None,
            branch: "main",
            head_sha: &admitted_sha,
            workflow_path: ".github/workflows/extra.yml",
            workflow_blobs: &json!({".github/workflows/extra.yml":blob}),
            workflow_inputs: None,
            environment: None,
            manual: true,
        },
        &[alice],
    )
    .await;
    assert!(refused
        .unwrap_err()
        .to_string()
        .contains("Workflow is disabled"));
    checked(
        &api,
        "actions/enable-workflow",
        "PUT",
        &format!("{base}/extra.yml/enable"),
        &contributor,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    // Repository write permission does not bypass the forge's host-execution trust policy.
    checked(
        &api,
        "",
        "POST",
        &format!("{base}/extra.yml/dispatches"),
        &contributor,
        json!({"ref":"main"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &api,
        "",
        "POST",
        &format!("{base}/extra.yml/dispatches"),
        &writer,
        json!({"ref":"missing"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &api,
        "",
        "POST",
        &format!("{base}/extra.yml/dispatches"),
        &writer,
        json!({"ref":"main","inputs":{"unknown":"value"}}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &api,
        "actions/create-workflow-dispatch",
        "POST",
        &format!("{base}/extra.yml/dispatches"),
        &writer,
        json!({"ref":"main"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].head_sha.as_deref(), Some(admitted_sha.as_str()));
    assert_eq!(runs[0].workflow_inputs, Some(json!({"text":"hello"})));
    assert_eq!(runs[0].workflow_blobs[".github/workflows/extra.yml"], blob);
    let (_, details) = checked(
        &api,
        "actions/create-workflow-dispatch",
        "POST",
        &format!("{base}/{id}/dispatches"),
        &writer,
        json!({"ref":"main","inputs":{"text":"submitted"},"return_run_details":true}),
        StatusCode::OK,
    )
    .await;
    let run_id = details["workflow_run_id"].as_i64().unwrap();
    let second = db::ci::get_ci_run_by_id(f.pool(), repo.id, run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.workflow_inputs, Some(json!({"text":"submitted"})));
    let (status, _, svg) = raw(&api, "GET", badge.path(), &reader, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8(svg).unwrap().contains("queued"));
    for _ in 0..2 {
        let run = db::ci::claim_ci_run(f.pool(), "badge-worker", "local", &[alice], &codeyard::backend::ci::CiConfig::default().platform_labels())
            .await
            .unwrap()
            .unwrap();
        db::ci::finish_ci_run(f.pool(), run.id, "badge-worker", "success", None)
            .await
            .unwrap()
            .unwrap();
    }
    let (_, _, svg) = raw(&api, "GET", badge.path(), &reader, Value::Null).await;
    assert!(String::from_utf8(svg).unwrap().contains("passing"));
    let (_, _, svg) = raw(
        &api,
        "GET",
        &format!("{}?branch=other", badge.path()),
        &reader,
        Value::Null,
    )
    .await;
    assert!(String::from_utf8(svg).unwrap().contains("no status"));
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_workflows WHERE repo_id=$1",
            &[repo.id]
        )
        .await,
        2
    );
    // A disabled automatic workflow must not turn an accepted smart-HTTP push into an error.
    checked(
        &api,
        "actions/disable-workflow",
        "PUT",
        &format!("{base}/ci.yml/disable"),
        &writer,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    f.execute("UPDATE users SET is_admin=true WHERE id=$1", &[alice])
        .await;
    {
        use crate::support::git_server::{self, git, success};
        let server = git_server::Server::start(root.path(), &f.connection_url()).await;
        success(git(
            root.path(),
            &[
                "clone",
                &format!("{}/alice/project.git", server.base),
                "checkout",
            ],
            Some(&writer),
        ));
        std::fs::write(
            root.path().join("checkout/new.txt"),
            "a push with disabled workflows",
        )
        .unwrap();
        success(git(
            root.path(),
            &["-C", "checkout", "add", "new.txt"],
            None,
        ));
        success(git(
            root.path(),
            &["-C", "checkout", "commit", "-m", "An accepted push"],
            None,
        ));
        success(git(
            root.path(),
            &["-C", "checkout", "push", "origin", "HEAD:main"],
            Some(&writer),
        ));
        drop(server);
    }
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1", &[repo.id])
            .await,
        2
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_actions_workflows() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_actions_workflows() {
    contract(true).await;
}
