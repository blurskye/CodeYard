use crate::backend::api::ci::jobs::authorized_run;
use crate::backend::api::ci::jobs::CiReadError;
use crate::backend::api::ci::responses::map_job_info;
use crate::backend::api::ci::responses::JobInfo;
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
