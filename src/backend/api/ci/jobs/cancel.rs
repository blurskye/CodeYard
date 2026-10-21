use super::{parse_job_id, CiReadError};
use crate::backend::{api::http::session::get_current_user, app::AppState};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Serialize)]
pub struct CancelCiResponse {
    pub requested: bool,
}

pub async fn cancel_ci_job(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Result<Json<CancelCiResponse>, CiReadError> {
    let user = get_current_user(&state.pool, &headers).await?;
    let requested = state
        .ci_runs
        .cancel_run(parse_job_id(&job_id)?, user.id)
        .await
        .map_err(super::super::admission_error)?;
    Ok(Json(CancelCiResponse { requested }))
}
