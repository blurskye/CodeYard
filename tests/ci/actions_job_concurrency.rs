//! Job-level concurrency: one job per group holds it, a newer job replaces the waiting one,
//! and cancel-in-progress cancels the holder, which keeps the group until it has stopped.
use super::{
    actions_objects::pat,
    actions_workflows::{checked, router},
};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use axum::{http::StatusCode, Router};
use codeyard::backend::db;
use codeyard_actions::progress::Conclusion;
use codeyard_actions::progress::Event;
use codeyard_actions::progress::Update;
use codeyard_app::ci::runtime;
use serde_json::json;

const WORKFLOW: &str = ".github/workflows/ci.yml";

struct Job {
    worker: &'static str,
    run: i64,
    generation: String,
    id: i64,
}

fn event(update: Update) -> Event {
    Event {
        job: "test".into(),
        variant: 0,
        at_ms: chrono::Utc::now().timestamp_millis() as u64,
        update,
    }
}

fn request(group: &str, cancel_in_progress: bool) -> Event {
    event(Update::Concurrency {
        group: group.into(),
        cancel_in_progress,
    })
}

/// Dispatches a run, claims it as `worker` and queues its job with a concurrency request.
async fn start(
    f: &Database,
    api: &Router,
    writer: &str,
    actor: i64,
    worker: &'static str,
    group: &str,
    cancel_in_progress: bool,
) -> Job {
    checked(
        api,
        "actions/create-workflow-dispatch",
        "POST",
        "/api/v3/repos/alice/project/actions/workflows/ci.yml/dispatches",
        writer,
        json!({"ref":"main","return_run_details":true}),
        StatusCode::OK,
    )
    .await;
    let run = db::ci::claim_ci_run(
        f.pool(),
        worker,
        "local",
        &[actor],
        &codeyard_app::ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .unwrap();
    let generation = runtime::token_hash(&format!("runtime-{worker}"));
    db::action_objects::issue_token(
        f.pool(),
        run.id,
        run.worker.as_deref().unwrap_or(""),
        &generation,
        &run.run_id.to_string(),
    )
    .await
    .unwrap();
    super::actions_objects::job_credentials(f, &run, &["test"]).await;
    let queued = event(Update::Queued {
        workflow_name: "Build".into(),
        name: "test".into(),
        labels: vec!["ubuntu-latest".into()],
        matrix: None,
    });
    let id = db::action_jobs::apply(f.pool(), worker, run.id, &generation, WORKFLOW, &queued)
        .await
        .unwrap()
        .id;
    db::action_jobs::apply(
        f.pool(),
        worker,
        run.id,
        &generation,
        WORKFLOW,
        &request(group, cancel_in_progress),
    )
    .await
    .unwrap();
    Job {
        worker,
        run: run.id,
        generation,
        id,
    }
}

async fn admit(f: &Database, job: &Job) -> Option<bool> {
    db::action_jobs::admit(
        f.pool(),
        job.worker,
        job.run,
        &job.generation,
        WORKFLOW,
        job.id,
    )
    .await
    .unwrap()
}

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
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

    let one = start(&f, &api, &writer, alice, "one", "Deploy", false).await;
    assert_eq!(admit(&f, &one).await, Some(true));
    let two = start(&f, &api, &writer, alice, "two", "Deploy", false).await;
    assert_eq!(admit(&f, &two).await, None);
    assert_eq!(admit(&f, &one).await, Some(true));

    // Group names compare without case. Cancel-in-progress cancels the holder and the
    // waiting job, but the holder keeps the group until it has stopped.
    let three = start(&f, &api, &writer, alice, "three", "deploy", true).await;
    assert_eq!(admit(&f, &three).await, None);
    assert_eq!(admit(&f, &two).await, Some(false));
    assert_eq!(admit(&f, &one).await, Some(false));
    assert!(
        db::action_jobs::admit(f.pool(), "two", one.run, &one.generation, WORKFLOW, one.id)
            .await
            .is_err()
    );
    assert!(db::action_jobs::admit(
        f.pool(),
        three.worker,
        three.run,
        &three.generation,
        WORKFLOW,
        one.id
    )
    .await
    .is_err());

    let finished = event(Update::Finished {
        conclusion: Conclusion::Cancelled,
        duration_ms: 0,
    });
    db::action_jobs::apply(
        f.pool(),
        one.worker,
        one.run,
        &one.generation,
        WORKFLOW,
        &finished,
    )
    .await
    .unwrap();
    assert_eq!(admit(&f, &three).await, Some(true));
    assert!(db::action_jobs::apply(
        f.pool(),
        one.worker,
        one.run,
        &one.generation,
        WORKFLOW,
        &request("Deploy", false),
    )
    .await
    .is_err());
    f.finish().await;
}

#[tokio::test]
async fn sqlite_job_concurrency() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_job_concurrency() {
    contract(true).await;
}
