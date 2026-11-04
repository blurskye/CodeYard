//! A native worker's authenticated observations survive completion and remain repository-scoped.
use super::{
    actions_objects::pat,
    actions_workflows::{checked, router},
};
use crate::support::{database::Database, fake_webui_bundle};
use axum::http::StatusCode;

use codeyard_actions::progress::{Conclusion, Event, JobResult, State, Update};
use codeyard_model::ci::LogLine;
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn event(variant: u32, update: Update) -> Event {
    Event {
        job: "build".into(),
        variant,
        at_ms: chrono::Utc::now().timestamp_millis() as u64,
        update,
    }
}
async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
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
    let other = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
        "other",
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
    let (_, dispatch) = checked(
        &api,
        "actions/create-workflow-dispatch",
        "POST",
        &format!("{base}/workflows/ci.yml/dispatches"),
        &writer,
        json!({"ref":"main","return_run_details":true}),
        StatusCode::OK,
    )
    .await;
    let run = codeyard_db_actions::ci::claim_ci_run(
        f.pool(),
        "jobs",
        "local",
        &[alice],
        &codeyard::backend::ci::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(dispatch["workflow_run_id"], run.id);
    let workflow = ".github/workflows/ci.yml";
    let generation = codeyard::backend::ci::runtime::token_hash("private-runtime");
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
    codeyard_db_actions::action_jobs::save_state(
        f.pool(),
        "jobs",
        run.id,
        &generation,
        workflow,
        &State::default(),
    )
    .await
    .unwrap();
    let queued = event(
        0,
        Update::Queued {
            workflow_name: "Build".into(),
            name: "Build Linux".into(),
            labels: vec!["linux".into()],
            matrix: Some(codeyard_actions::progress::Matrix {
                values: json!({"os":"linux"}),
            }),
        },
    );
    assert!(codeyard_db_actions::action_jobs::apply(
        f.pool(),
        "other",
        run.id,
        &generation,
        workflow,
        &queued
    )
    .await
    .is_err());
    assert!(codeyard_db_actions::action_jobs::apply(
        f.pool(),
        "jobs",
        run.id,
        "stale",
        workflow,
        &queued
    )
    .await
    .is_err());
    assert!(codeyard_db_actions::action_jobs::apply(
        f.pool(),
        "jobs",
        run.id,
        &generation,
        ".github/workflows/foreign.yml",
        &queued
    )
    .await
    .is_err());
    let job = codeyard_db_actions::action_jobs::apply(
        f.pool(),
        "jobs",
        run.id,
        &generation,
        workflow,
        &queued,
    )
    .await
    .unwrap()
    .id;
    assert!(
        codeyard_db_actions::action_jobs::get(f.pool(), repo.id, job, false)
            .await
            .unwrap()
            .is_none()
    );
    let (_, empty) = checked(
        &api,
        "actions/list-jobs-for-workflow-run",
        "GET",
        &format!("{base}/runs/{}/jobs", run.id),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(empty["total_count"], 0);
    for update in [
        Update::Started,
        Update::StepStarted {
            number: 1,
            name: "Run build".into(),
        },
    ] {
        assert_eq!(
            codeyard_db_actions::action_jobs::apply(
                f.pool(),
                "jobs",
                run.id,
                &generation,
                workflow,
                &event(0, update)
            )
            .await
            .unwrap()
            .id,
            job
        );
    }
    assert_eq!(
        codeyard_db_actions::action_jobs::get(f.pool(), repo.id, job, false)
            .await
            .unwrap()
            .unwrap()
            .matrix
            .unwrap()
            .0,
        json!({"os":"linux"})
    );
    assert!(
        codeyard_db_actions::action_jobs::get(f.pool(), other.id, job, false)
            .await
            .unwrap()
            .is_none()
    );
    assert!(codeyard_db_actions::ci::append_ci_logs(
        f.pool(),
        run.id,
        "jobs",
        0,
        &[LogLine {
            stage: workflow.into(),
            line: "observed output".into(),
            level: "info".into(),
            job_id: Some(job),
            step_number: Some(1)
        }]
    )
    .await
    .unwrap());
    assert!(codeyard_db_actions::ci::append_ci_logs(
        f.pool(),
        run.id,
        "jobs",
        1,
        &[LogLine {
            stage: workflow.into(),
            line: "wrong step".into(),
            level: "info".into(),
            job_id: Some(job),
            step_number: Some(2)
        }]
    )
    .await
    .is_err());
    assert!(codeyard_db_actions::ci::append_ci_logs(
        f.pool(),
        run.id,
        "jobs",
        1,
        &[LogLine {
            stage: workflow.into(),
            line: "wrong job".into(),
            level: "info".into(),
            job_id: Some(job + 10000),
            step_number: None
        }]
    )
    .await
    .is_err());
    codeyard_db_actions::action_jobs::apply(
        f.pool(),
        "jobs",
        run.id,
        &generation,
        workflow,
        &event(
            0,
            Update::StepFinished {
                number: 1,
                conclusion: Conclusion::Success,
                duration_ms: 17,
            },
        ),
    )
    .await
    .unwrap();
    let second = codeyard_db_actions::action_jobs::apply(
        f.pool(),
        "jobs",
        run.id,
        &generation,
        workflow,
        &event(
            1,
            Update::Queued {
                workflow_name: "Build".into(),
                name: "Build Mac".into(),
                labels: vec!["macos".into()],
                matrix: Some(codeyard_actions::progress::Matrix {
                    values: json!({"os":"macos"}),
                }),
            },
        ),
    )
    .await
    .unwrap()
    .id;
    codeyard_db_actions::action_jobs::apply(
        f.pool(),
        "jobs",
        run.id,
        &generation,
        workflow,
        &event(1, Update::Started),
    )
    .await
    .unwrap();
    codeyard_db_actions::action_jobs::apply(
        f.pool(),
        "jobs",
        run.id,
        &generation,
        workflow,
        &event(
            1,
            Update::StepStarted {
                number: 1,
                name: "Interrupted".into(),
            },
        ),
    )
    .await
    .unwrap();
    let (headers, page) = checked(
        &api,
        "actions/list-jobs-for-workflow-run",
        "GET",
        &format!("{base}/runs/{}/jobs?per_page=1", run.id),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(page["total_count"], 2);
    assert_eq!(page["jobs"][0]["id"], job);
    assert!(headers.contains_key("link"));
    let (_, active) = checked(
        &api,
        "actions/get-job-for-workflow-run",
        "GET",
        &format!("{base}/jobs/{job}"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_gh_job(
        &active,
        run.id,
        job,
        "Build Linux",
        "in_progress",
        Value::Null,
    );
    assert_eq!(active["steps"][0]["name"], "Run build");
    assert_eq!(active["steps"][0]["number"], 1);
    assert_eq!(active["steps"][0]["status"], "completed");
    for field in ["started_at", "completed_at"] {
        chrono::DateTime::parse_from_rfc3339(active["steps"][0][field].as_str().unwrap()).unwrap();
    }
    assert_eq!(active["status"], "in_progress");
    assert_eq!(active["steps"][0]["conclusion"], "success");
    assert_eq!(active["runner_id"], Value::Null);
    assert!(active["check_run_url"]
        .as_str()
        .unwrap()
        .contains("/check-runs/"));
    assert!(!active.to_string().contains(&generation));
    assert!(!active.to_string().contains("private-runtime"));
    for (path, token) in [
        (
            format!("/api/v3/repos/alice/other/actions/jobs/{job}"),
            &reader,
        ),
        (format!("{base}/jobs/{job}"), &outsider),
    ] {
        checked(
            &api,
            "",
            "GET",
            &path,
            token,
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    annotations(
        &f,
        &api,
        (run.id, job, other.id),
        &generation,
        (&reader, &outsider),
    )
    .await;
    summaries(&f, (run.id, job, repo.id, other.id), &generation).await;
    let mut state = State::default();
    state.jobs.insert(
        "unobserved".into(),
        JobResult {
            result: "success".into(),
            outputs: BTreeMap::new(),
        },
    );
    assert!(codeyard_db_actions::action_jobs::save_state(
        f.pool(),
        "jobs",
        run.id,
        &generation,
        workflow,
        &state
    )
    .await
    .is_err());
    state.jobs.clear();
    state.variants.insert(
        "build".into(),
        BTreeMap::from([(
            0,
            JobResult {
                result: "success".into(),
                outputs: BTreeMap::from([("answer".into(), "real output".into())]),
            },
        )]),
    );
    assert!(codeyard_db_actions::action_jobs::save_state(
        f.pool(),
        "jobs",
        run.id,
        &generation,
        workflow,
        &state
    )
    .await
    .is_err());
    codeyard_db_actions::action_jobs::apply(
        f.pool(),
        "jobs",
        run.id,
        &generation,
        workflow,
        &event(
            0,
            Update::Finished {
                conclusion: Conclusion::Success,
                duration_ms: 81,
            },
        ),
    )
    .await
    .unwrap();
    codeyard_db_actions::action_jobs::save_state(
        f.pool(),
        "jobs",
        run.id,
        &generation,
        workflow,
        &state,
    )
    .await
    .unwrap();
    codeyard_db_actions::ci::finish_ci_run(f.pool(), run.id, "jobs", "cancelled", None)
        .await
        .unwrap()
        .unwrap();
    assert!(codeyard_db_actions::action_jobs::apply(
        f.pool(),
        "jobs",
        run.id,
        &generation,
        workflow,
        &event(0, Update::Started)
    )
    .await
    .is_err());
    let (_, complete) = checked(
        &api,
        "actions/get-job-for-workflow-run",
        "GET",
        &format!("{base}/jobs/{job}"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_gh_job(
        &complete,
        run.id,
        job,
        "Build Linux",
        "completed",
        json!("success"),
    );
    assert_eq!(complete["status"], "completed");
    assert_eq!(complete["conclusion"], "success");
    let (_, interrupted) = checked(
        &api,
        "actions/get-job-for-workflow-run",
        "GET",
        &format!("{base}/jobs/{second}"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(interrupted["status"], "completed");
    assert_eq!(interrupted["conclusion"], Value::Null);
    assert_eq!(interrupted["steps"][0]["conclusion"], Value::Null);
    let (_, attempt) = checked(
        &api,
        "actions/list-jobs-for-workflow-run-attempt",
        "GET",
        &format!("{base}/runs/{}/attempts/1/jobs?per_page=100", run.id),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(attempt["total_count"], 2);
    assert_eq!(attempt["jobs"][0], complete);
    let (_, watch) = checked(
        &api,
        "actions/list-jobs-for-workflow-run",
        "GET",
        &format!("{base}/runs/{}/jobs?per_page=100", run.id),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(watch["total_count"], 2);
    assert_eq!(watch["jobs"][0], complete);
    checked(
        &api,
        "",
        "GET",
        &format!("{base}/runs/{}/jobs?filter=unknown", run.id),
        &reader,
        Value::Null,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &api,
        "",
        "GET",
        &format!("{base}/runs/{}/attempts/2/jobs", run.id),
        &reader,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM ci_log_chunks WHERE run_id=$1",
            &[run.id]
        )
        .await,
        1
    );
    super::actions_logs::contract(&f, &api, base, run.id, job, &reader, &writer).await;
    checked(
        &api,
        "actions/delete-workflow-run",
        "DELETE",
        &format!("{base}/runs/{}", run.id),
        &writer,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert!(
        codeyard_db_actions::action_jobs::get(f.pool(), repo.id, job, false)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM actions_steps WHERE job_id=$1", &[job])
            .await,
        0
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_job_states WHERE run_id=$1",
            &[run.id]
        )
        .await,
        0
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_job_annotations WHERE job_id=$1",
            &[job]
        )
        .await,
        0
    );
    f.finish().await;
}

pub(super) fn assert_gh_job(
    value: &Value,
    run: i64,
    job: i64,
    name: &str,
    status: &str,
    conclusion: Value,
) {
    assert_eq!(value["id"], job);
    assert_eq!(value["run_id"], run);
    assert_eq!(value["name"], name);
    assert_eq!(value["status"], status);
    assert_eq!(value["conclusion"], conclusion);
    let url = url::Url::parse(value["html_url"].as_str().unwrap()).unwrap();
    assert_eq!(url.path(), "/alice/project/actions");
    assert!(url.query().unwrap().starts_with("job="));
    assert!(url
        .query_pairs()
        .any(|(key, value)| key == "workflow_job" && value == name));
    chrono::DateTime::parse_from_rfc3339(value["started_at"].as_str().unwrap()).unwrap();
    if status == "completed" {
        chrono::DateTime::parse_from_rfc3339(value["completed_at"].as_str().unwrap()).unwrap();
    } else {
        assert_eq!(value["completed_at"], Value::Null);
    }
}

async fn summaries(f: &Database, (run, job, repo, other): (i64, i64, i64, i64), generation: &str) {
    let send = |worker: &'static str, number: u32, markdown: &str| {
        let update = Update::Summary {
            number,
            markdown: markdown.into(),
        };
        async move {
            codeyard_db_actions::action_jobs::apply(
                f.pool(),
                worker,
                run,
                generation,
                ".github/workflows/ci.yml",
                &event(0, update),
            )
            .await
        }
    };
    assert!(send("unowned", 1, "### Stolen").await.is_err());
    assert!(send("jobs", 0, "### Zero").await.is_err());
    assert!(send("jobs", 1, "### Nul\0").await.is_err());
    assert!(send(
        "jobs",
        1,
        "x".repeat(codeyard_actions::progress::MAX_STEP_SUMMARY_BYTES + 1)
            .as_str()
    )
    .await
    .is_err());
    assert_eq!(send("jobs", 1, "### First").await.unwrap().id, job);
    // A later report for the same step replaces the earlier one.
    assert_eq!(
        send("jobs", 1, "### Build\nall green").await.unwrap().id,
        job
    );
    let stored = codeyard_db_actions::action_jobs::summaries(f.pool(), repo, run)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].number, 1);
    assert_eq!(stored[0].markdown, "### Build\nall green");
    assert!(
        codeyard_db_actions::action_jobs::summaries(f.pool(), other, run)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn annotations(
    f: &Database,
    api: &axum::Router,
    (run, job, other): (i64, i64, i64),
    generation: &str,
    (reader, outsider): (&str, &str),
) {
    use codeyard_actions::progress::Annotation;
    let annotation = Annotation {
        path: "src/main.rs".into(),
        start_line: 7,
        end_line: 9,
        start_column: None,
        end_column: None,
        annotation_level: "failure".into(),
        message: "Compiler diagnostic\nsecond line".into(),
        title: Some("Build".into()),
    };
    let second = Annotation {
        start_line: 12,
        end_line: 12,
        ..annotation.clone()
    };
    // A repeated annotation is stored once, so the job keeps two.
    for (worker, sent) in [
        ("unowned", &annotation),
        ("jobs", &annotation),
        ("jobs", &annotation),
        ("jobs", &second),
    ] {
        let result = codeyard_db_actions::action_jobs::apply(
            f.pool(),
            worker,
            run,
            generation,
            ".github/workflows/ci.yml",
            &event(
                0,
                Update::Annotation {
                    annotation: sent.clone(),
                },
            ),
        )
        .await;
        if worker == "unowned" {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap().id, job);
        }
    }
    assert!(
        codeyard_db_actions::action_jobs::annotations(f.pool(), other, job, 0, 100)
            .await
            .unwrap()
            .1
            .is_empty()
    );
    let check = f
        .scalar("SELECT check_run_id FROM actions_jobs WHERE id=$1", &[job])
        .await;
    let path = format!("/api/v3/repos/alice/project/check-runs/{check}/annotations");
    let (_, body) = checked(
        api,
        "checks/list-annotations",
        "GET",
        &path,
        reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 2);
    assert_eq!(body[0]["message"], annotation.message);
    assert_eq!(body[0]["path"], annotation.path);
    assert_eq!(body[0]["annotation_level"], "failure");
    assert_eq!(body[0]["start_line"], 7);
    assert_eq!(body[0]["end_line"], 9);
    let (headers, first) = checked(
        api,
        "checks/list-annotations",
        "GET",
        &format!("{path}?per_page=1"),
        reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    assert_eq!(first[0], body[0]);
    checked(
        api,
        "",
        "GET",
        &path,
        outsider,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        api,
        "",
        "GET",
        &path.replace("/project/", "/other/"),
        reader,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
}
#[tokio::test]
async fn sqlite_actions_jobs() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_actions_jobs() {
    contract(true).await;
}
