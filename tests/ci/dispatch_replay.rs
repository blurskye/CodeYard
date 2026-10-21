//! Dispatch form limits and reruns retain the same workflow inputs and files.
use crate::support::{database::Database, fake_webui_bundle as bundle, http_client::Client};
use axum::http::StatusCode;
use base64::Engine;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, repositories::lifecycle,
    storage::ObjectStore,
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc};

async fn contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    db::node_config::upsert_node_configuration(f.pool(), "public", true)
        .await
        .unwrap();
    let actor = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let config = CiConfig {
        trusted_actor_ids: BTreeSet::from([actor]),
        ..CiConfig::default()
    };
    let objects = Arc::new(ObjectStore::open(root.path(), None).await.unwrap());
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        objects.clone(),
        None,
        config.clone(),
        None,
    ));
    let client = Client::login(&router, f.pool(), actor).await;
    let workflow = "on: workflow_dispatch\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{run: echo ok}]\n";
    let mut tip = "unborn".to_owned();
    for filename in ["ci.yml", "deploy-image.yml"] {
        let saved = client.expect("POST", &format!("/api/repos/alice/proj/contents/.github/workflows/{filename}"), json!({
            "content": base64::engine::general_purpose::STANDARD.encode(workflow), "message": "Workflow", "branch": "main", "expected_revision": tip,
        }), StatusCode::CREATED).await;
        tip = saved["sha"].as_str().unwrap().to_owned();
    }
    let dispatched = client.expect("POST", "/api/ci/trigger", json!({
        "repo_owner": "alice", "repo_name": "proj", "branch": "main", "event": "workflow_dispatch", "pipeline": "full",
    }), StatusCode::OK).await;
    let first = db::ci::get_ci_run_by_run_id(
        f.pool(),
        dispatched["job_id"].as_str().unwrap().parse().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        first.workflow_path,
        ".github/workflows/ci.yml,.github/workflows/deploy-image.yml"
    );
    let changed = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        objects,
        None,
        CiConfig {
            workflow_ci: ".github/workflows/lint.yml".into(),
            workflow_cd: ".github/workflows/release.yml".into(),
            ..config
        },
        None,
    ));
    let current = Client::login(&changed, f.pool(), actor).await;
    let replay = current
        .form_reply(
            &format!("/alice/proj/actions/runs/{}/rerun", first.run_id),
            &[],
        )
        .await;
    assert_eq!(replay.status, StatusCode::SEE_OTHER, "{}", replay.text());
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 2);
    let again = runs.iter().find(|run| run.id != first.id).unwrap();
    assert_eq!(again.workflow_path, first.workflow_path);
    assert_eq!(again.head_sha, first.head_sha);
    assert_eq!(again.workflow_blobs, first.workflow_blobs);

    // Five valid values exceed the old 8 KiB global-only cap.
    let declarations = (0..5)
        .map(|i| format!("      value{i}:\n        required: true\n        type: string\n"))
        .collect::<String>();
    let inputs_workflow = format!("on:\n  workflow_dispatch:\n    inputs:\n{declarations}jobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{{run: echo ok}}]\n");
    client.expect("POST", "/api/repos/alice/proj/contents/.github/workflows/manual.yml", json!({
        "content": base64::engine::general_purpose::STANDARD.encode(inputs_workflow), "message": "Inputs", "branch": "main", "expected_revision": tip,
    }), StatusCode::CREATED).await;
    let text = (0..5)
        .map(|i| format!("value{i}={}", "x".repeat(1700)))
        .collect::<Vec<_>>()
        .join("\r\n");
    let reply = client
        .form_reply(
            "/ci/dispatch",
            &[
                ("repo_owner", "alice"),
                ("repo_name", "proj"),
                ("branch", "main"),
                ("event", "workflow_dispatch"),
                ("pipeline", "ci"),
                ("workflow_path", ".github/workflows/manual.yml"),
                ("inputs", &text),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
        .await
        .unwrap();
    let manual = runs
        .iter()
        .find(|run| run.workflow_path == ".github/workflows/manual.yml")
        .unwrap();
    let values = manual
        .workflow_inputs
        .as_ref()
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(values.len(), 5);
    assert!(values
        .values()
        .all(|value| value.as_str().unwrap().len() == 1700));
    // Per-value validation still refuses a value over the shared bound.
    let oversized = text.replacen(&"x".repeat(1700), &"x".repeat(2049), 1);
    let refused = client
        .form_reply(
            "/ci/dispatch",
            &[
                ("repo_owner", "alice"),
                ("repo_name", "proj"),
                ("branch", "main"),
                ("event", "workflow_dispatch"),
                ("pipeline", "ci"),
                ("workflow_path", ".github/workflows/manual.yml"),
                ("inputs", &oversized),
            ],
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    let _ = client
        .expect(
            "GET",
            "/api/ci/jobs?owner=alice&repo=proj",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
            .await
            .unwrap()
            .len(),
        3
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_reruns_keep_recorded_workflows_and_global_inputs_match_shared_limits() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_reruns_keep_recorded_workflows_and_global_inputs_match_shared_limits() {
    contract(true).await;
}
