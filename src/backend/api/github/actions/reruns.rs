//! New attempts use native immutable admission and observed dependency state.
use super::workflows::admission_error;
use crate::backend::{
    api::github::{auth, repository, Error, Result},
    app::AppState,
};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Default, Deserialize)]
struct Parameters {
    #[serde(default)]
    enable_debug_logging: bool,
}
#[derive(Clone, Copy)]
enum Selection {
    All,
    Failed,
    Job,
}

async fn perform(
    state: Arc<AppState>,
    headers: HeaderMap,
    (owner, name, id): (String, String, i64),
    body: Bytes,
    selection: Selection,
) -> Result<Response> {
    let (repo, authentication) = repository(&state, &headers, &owner, &name, true).await?;
    let authentication = authentication.ok_or_else(auth::required)?;
    let actor = authentication.user.id;
    let credential = Some(authentication.token.id);
    let args: Parameters = if body.is_empty() {
        Parameters::default()
    } else {
        serde_json::from_slice::<Option<Parameters>>(&body)
            .map_err(|_| Error::invalid_simple("Invalid re-run parameters"))?
            .unwrap_or_default()
    };
    match selection {
        Selection::All => {
            state
                .ci_runs
                .rerun_workflow_run(repo.id, id, actor, args.enable_debug_logging, credential)
                .await
        }
        Selection::Failed => {
            state
                .ci_runs
                .rerun_failed_workflow_run(
                    repo.id,
                    id,
                    actor,
                    args.enable_debug_logging,
                    credential,
                )
                .await
        }
        Selection::Job => {
            state
                .ci_runs
                .rerun_workflow_job(repo.id, id, actor, args.enable_debug_logging, credential)
                .await
        }
    }
    .map_err(admission_error)?;
    Ok((StatusCode::CREATED, Json(json!({}))).into_response())
}
async fn all(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<(String, String, i64)>,
    body: Bytes,
) -> Result<Response> {
    perform(state, headers, path, body, Selection::All).await
}
async fn failed(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<(String, String, i64)>,
    body: Bytes,
) -> Result<Response> {
    perform(state, headers, path, body, Selection::Failed).await
}
async fn job(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<(String, String, i64)>,
    body: Bytes,
) -> Result<Response> {
    perform(state, headers, path, body, Selection::Job).await
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/re-run-workflow", post(all)),
        ("actions/re-run-workflow-failed-jobs", post(failed)),
        ("actions/re-run-job-for-workflow-run", post(job)),
    ]
}
