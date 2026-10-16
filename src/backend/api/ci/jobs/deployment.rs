use super::{authorized_run, unavailable, CiReadError};
use crate::backend::{api::error::ApiError, app::AppState, db};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
pub struct DeploymentInfo {
    pub environment: String,
    pub status: String,
    pub approved_by_id: Option<i64>,
    pub approved_at: Option<i64>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
}

pub async fn get_ci_deployment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Result<Json<Option<DeploymentInfo>>, CiReadError> {
    let run = authorized_run(&state, &headers, &job_id).await?;
    let deployment = db::ci::get_ci_deployment_by_run_id(&state.pool, run.id)
        .await
        .map_err(unavailable)?;
    if deployment.is_none()
        && (matches!(run.pipeline.as_str(), "cd" | "full") || run.environment.is_some())
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::new(
                "deployment_unavailable",
                "Expected deployment state is missing",
            )),
        ));
    }
    Ok(Json(deployment.map(|row| DeploymentInfo {
        environment: row.environment,
        status: row.status,
        approved_by_id: row.approved_by_id,
        approved_at: row.approved_at.map(|v| v.timestamp()),
        started_at: row.started_at.map(|v| v.timestamp()),
        completed_at: row.completed_at.map(|v| v.timestamp()),
    })))
}
