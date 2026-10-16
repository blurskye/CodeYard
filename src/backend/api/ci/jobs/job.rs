use super::super::responses::{map_job_info, JobInfo};
use super::{authorized_run, CiReadError};
use crate::backend::app::AppState;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use std::sync::Arc;

pub async fn get_ci_job(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Result<Json<JobInfo>, CiReadError> {
    Ok(Json(map_job_info(
        authorized_run(&state, &headers, &job_id).await?,
    )))
}
