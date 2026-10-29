//! Cancellation and deletion retain the worker's authority, cleanup, and notification paths.
use super::{run, workflows::admission_error};
use crate::backend::{
    api::github::{auth, repository, Error, Result},
    app::AppState,
    db::{self, ci::CiRunDeletion},
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, post, MethodRouter},
    Json,
};
use serde_json::json;
use std::sync::Arc;

async fn cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, authentication) = repository(&state, &headers, &owner, &name, true).await?;
    let authentication = authentication.ok_or_else(auth::required)?;
    let run = run(&state, repo.id, id).await?;
    // Both engines use bounded process-group termination, rather than leaving always() jobs running.
    if !state
        .ci_runs
        .cancel_run(
            run.run_id,
            authentication.workflow_actor(&state, repo.id).await?,
            Some(authentication.ci_credential(&headers, "actions")?),
        )
        .await
        .map_err(admission_error)?
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "Cannot cancel a completed workflow run",
        ));
    }
    Ok((StatusCode::ACCEPTED, Json(json!({}))).into_response())
}

async fn remove(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    id: i64,
    kind: CiRunDeletion,
) -> Result<StatusCode> {
    let (repo, authentication) = repository(state, headers, owner, name, true).await?;
    let actor = authentication.ok_or_else(auth::required)?.user.id;
    if !db::ci::delete_ci_run_data(&state.pool, actor, repo.id, id, kind)
        .await
        .map_err(|error| admission_error(error.into()))?
    {
        return Err(Error::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_run(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    remove(&state, &headers, &owner, &name, id, CiRunDeletion::Run).await
}
async fn delete_logs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    remove(&state, &headers, &owner, &name, id, CiRunDeletion::Logs).await
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/cancel-workflow-run", post(cancel)),
        ("actions/force-cancel-workflow-run", post(cancel)),
        ("actions/delete-workflow-run", delete(delete_run)),
        ("actions/delete-workflow-run-logs", delete(delete_logs)),
    ]
}
