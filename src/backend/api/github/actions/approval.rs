//! Fork approval uses the native pull request head check and execution authority.
use crate::backend::api::github::actions::run;

use crate::backend::api::github::issues::pull_error;
use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use crate::backend::repositories::pull_requests::workflows::run_pull_request_workflows;
use crate::backend::repositories::pull_requests::workflows::RunWorkflowsRequest;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{post, MethodRouter},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

async fn approve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<(StatusCode, Json<Value>)> {
    let (repo, authentication) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = authentication
        .ok_or_else(crate::backend::api::github::auth::required)?
        .user
        .id;
    let run = run(&state, repo.id, id).await?;
    if !run.approval_required || run.status != "queued" {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Workflow run is not awaiting approval",
        ));
    }
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
        &state.pool,
        run.pull_request_id.ok_or_else(Error::missing)?,
    )
    .await
    .map_err(Error::unavailable)?
    .filter(|pull| pull.repo_id == repo.id)
    .ok_or_else(Error::missing)?;
    run_pull_request_workflows(
        &state.pool,
        &state.ci_runs,
        actor,
        &repo,
        pull.number,
        RunWorkflowsRequest {
            expected_head: run.head_sha.ok_or_else(Error::missing)?,
        },
    )
    .await
    .map_err(pull_error)?;
    // GitHub answers 201 with an empty object.
    Ok((StatusCode::CREATED, Json(json!({}))))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("actions/approve-workflow-run", post(approve))]
}
