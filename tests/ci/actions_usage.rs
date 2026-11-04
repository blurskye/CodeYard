//! Timing includes immutable re-run attempts and excludes unobserved and self-hosted jobs.
use super::{
    actions_objects::pat,
    actions_workflows::{checked, router},
};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use axum::http::StatusCode;

use codeyard_actions::progress::Conclusion;
use codeyard_actions::progress::Event;
use codeyard_actions::progress::Update;

use serde_json::{json, Value};

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
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
    let api = router(&f, root.path(), alice).await;
    let writer = pat(&f, &api, alice, "write").await;
    let reader = pat(&f, &api, alice, "read").await;
    let outsider = pat(&f, &api, bob, "read").await;
    let base = "/api/v3/repos/alice/project/actions";
    let (_, dispatched) = checked(
        &api,
        "actions/create-workflow-dispatch",
        "POST",
        &format!("{base}/workflows/ci.yml/dispatches"),
        &writer,
        json!({"ref":"main","return_run_details":true}),
        StatusCode::OK,
    )
    .await;
    let id = dispatched["workflow_run_id"].as_i64().unwrap();
    let (_, empty) = checked(
        &api,
        "actions/get-workflow-run-usage",
        "GET",
        &format!("{base}/runs/{id}/timing"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(empty, json!({"billable":{},"run_duration_ms":0}));
    let mut expected_jobs = Vec::new();
    for attempt in 0..2 {
        let run = codeyard_db_actions::ci::claim_ci_run(
            f.pool(),
            "usage",
            "local",
            &[alice],
            &codeyard_app::ci::config::CiConfig::default().platform_labels(),
        )
        .await
        .unwrap()
        .unwrap();
        let generation = codeyard_support::token_hash::token_hash(&format!("usage-{attempt}"));
        codeyard_db_actions::action_objects::issue_token(
            f.pool(),
            run.id,
            run.worker.as_deref().unwrap_or(""),
            &generation,
            &run.run_id.to_string(),
        )
        .await
        .unwrap();
        super::actions_objects::job_credentials(&f, &run, &["build"]).await;
        for (variant, labels, duration) in [
            (0, vec!["ubuntu-latest".into()], 1234),
            (1, vec!["windows-2022".into()], 999),
            (2, vec!["self-hosted".into(), "ubuntu-latest".into()], 5000),
        ] {
            let mut job_id = 0;
            for update in [
                Update::Queued {
                    name: "Build".into(),
                    workflow_name: "Build".into(),
                    labels,
                    matrix: None,
                },
                Update::Started,
                Update::Finished {
                    conclusion: Conclusion::Success,
                    duration_ms: duration,
                },
            ] {
                job_id = codeyard_db_actions::action_jobs::apply(
                    f.pool(),
                    "usage",
                    run.id,
                    &generation,
                    ".github/workflows/ci.yml",
                    &Event {
                        job: "build".into(),
                        variant,
                        at_ms: chrono::Utc::now().timestamp_millis() as u64,
                        update,
                    },
                )
                .await
                .unwrap()
                .id;
            }
            if variant == 0 {
                expected_jobs.push(json!({"job_id":job_id,"duration_ms":1234}));
            }
        }
        codeyard_db_actions::ci::finish_ci_run(f.pool(), run.id, "usage", "success", None)
            .await
            .unwrap()
            .unwrap();
        if attempt == 0 {
            checked(
                &api,
                "actions/re-run-workflow",
                "POST",
                &format!("{base}/runs/{id}/rerun"),
                &writer,
                json!({}),
                StatusCode::CREATED,
            )
            .await;
        }
    }
    let (_, timing) = checked(
        &api,
        "actions/get-workflow-run-usage",
        "GET",
        &format!("{base}/runs/{id}/timing"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        timing["billable"]["UBUNTU"],
        json!({"total_ms":2468,"jobs":2,"job_runs":expected_jobs})
    );
    assert_eq!(timing["billable"]["WINDOWS"]["total_ms"], 1998);
    assert_eq!(timing["billable"].as_object().unwrap().len(), 2);
    assert!(timing["run_duration_ms"].as_i64().unwrap() >= 0);
    f.execute(
        "UPDATE actions_jobs SET started_at='2000-01-01T00:00:00Z' WHERE run_id=$1",
        &[id],
    )
    .await;
    let (_, workflow) = checked(
        &api,
        "actions/get-workflow-usage",
        "GET",
        &format!("{base}/workflows/ci.yml/timing"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        workflow,
        json!({"billable":{"UBUNTU":{"total_ms":1234},"WINDOWS":{"total_ms":999}}})
    );
    for (operation, path) in [
        (
            "actions/get-workflow-run-usage",
            format!("{base}/runs/{id}/timing"),
        ),
        (
            "actions/get-workflow-usage",
            format!("{base}/workflows/ci.yml/timing"),
        ),
    ] {
        checked(
            &api,
            operation,
            "GET",
            &path,
            &outsider,
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    checked(
        &api,
        "actions/get-workflow-run-usage",
        "GET",
        &format!("{base}/runs/999999/timing"),
        &reader,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn actions_usage_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn actions_usage_postgres() {
    contract(true).await;
}
