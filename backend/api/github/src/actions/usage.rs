//! Timing uses recorded executions, including re-runs, without minute rounding or multipliers.
use crate::actions::run;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use chrono::Datelike;
use chrono::TimeZone;
use chrono::Utc;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::json;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

fn document(jobs: Vec<codeyard_db_actions::action_jobs::Usage>, elapsed: Option<i64>) -> Value {
    let details = elapsed.is_some();
    let mut platforms: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for job in jobs {
        let labels: Vec<_> = job
            .labels
            .iter()
            .map(|label| label.to_ascii_lowercase())
            .collect();
        if labels.iter().any(|label| label == "self-hosted") {
            continue;
        }
        let platform = if labels.iter().any(|label| label.starts_with("ubuntu-")) {
            "UBUNTU"
        } else if labels.iter().any(|label| label.starts_with("windows-")) {
            "WINDOWS"
        } else if labels.iter().any(|label| label.starts_with("macos-")) {
            "MACOS"
        } else {
            continue;
        };
        platforms
            .entry(platform)
            .or_default()
            .push(json!({"job_id":job.id,"duration_ms":job.duration_ms}));
    }
    let billable: serde_json::Map<String, Value> = platforms
        .into_iter()
        .map(|(platform, jobs)| {
            let total: i64 = jobs
                .iter()
                .filter_map(|job| job["duration_ms"].as_i64())
                .sum();
            let value = if details {
                json!({"total_ms":total,"jobs":jobs.len(),"job_runs":jobs})
            } else {
                json!({"total_ms":total})
            };
            (platform.to_owned(), value)
        })
        .collect();
    let mut value = json!({"billable":billable});
    if let Some(elapsed) = elapsed {
        value["run_duration_ms"] = json!(elapsed);
    }
    value
}

async fn run_usage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let run = run(&state, repo.id, id).await?;
    let jobs = codeyard_db_actions::action_jobs::usage(
        &state.pool,
        repo.id,
        Some(run.workflow_run_id.unwrap_or(run.id)),
        None,
        chrono::DateTime::UNIX_EPOCH,
    )
    .await
    .map_err(Error::unavailable)?;
    let elapsed = codeyard_db_actions::action_jobs::elapsed(
        &state.pool,
        repo.id,
        run.workflow_run_id.unwrap_or(run.id),
    )
    .await
    .map_err(Error::unavailable)?;
    Ok(Json(document(jobs, Some(elapsed))))
}

async fn workflow_usage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, key)): Path<(String, String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let workflow = crate::actions::workflows::lookup_read(&state, &repo, &key).await?;
    let now = Utc::now();
    let since = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .ok_or_else(|| Error::unavailable(()))?;
    let jobs = codeyard_db_actions::action_jobs::usage(
        &state.pool,
        repo.id,
        None,
        Some(&workflow.path),
        since,
    )
    .await
    .map_err(Error::unavailable)?;
    Ok(Json(document(jobs, None)))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/get-workflow-run-usage", get(run_usage)),
        ("actions/get-workflow-usage", get(workflow_usage)),
    ]
}
