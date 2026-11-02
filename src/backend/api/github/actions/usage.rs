//! Timing uses recorded executions, including re-runs, without minute rounding or multipliers.
use super::{run, workflows};
use crate::backend::{
    api::github::{repository, Error, Result},
    app::AppState,
    db,
};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, MethodRouter},
    Json,
};
use chrono::{Datelike, TimeZone, Utc};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

fn document(jobs: Vec<db::action_jobs::Usage>, elapsed: Option<i64>) -> Value {
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
    let jobs = db::action_jobs::usage(
        &state.pool,
        repo.id,
        Some(run.workflow_run_id.unwrap_or(run.id)),
        None,
        chrono::DateTime::UNIX_EPOCH,
    )
    .await
    .map_err(Error::unavailable)?;
    let elapsed =
        db::action_jobs::elapsed(&state.pool, repo.id, run.workflow_run_id.unwrap_or(run.id))
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
    let workflow = workflows::lookup_read(&state, &repo, &key).await?;
    let now = Utc::now();
    let since = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .ok_or_else(|| Error::unavailable(()))?;
    let jobs = db::action_jobs::usage(&state.pool, repo.id, None, Some(&workflow.path), since)
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
