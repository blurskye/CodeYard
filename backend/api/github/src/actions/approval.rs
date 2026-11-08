//! Fork approval uses the native pull request head check and execution authority.
use crate::actions::run;
use crate::issues::pull_error;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_ci::pull_requests::workflows::run_pull_request_workflows;
use codeyard_ci::pull_requests::workflows::RunWorkflowsRequest;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

async fn approve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<(StatusCode, Json<Value>)> {
    let (repo, authentication) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = authentication
        .ok_or_else(codeyard_identity::github_auth::required)?
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
