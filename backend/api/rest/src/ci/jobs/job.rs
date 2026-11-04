use crate::ci::jobs::authorized_run;
use crate::ci::responses::map_job_info;
use crate::ci::responses::JobInfo;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use codeyard_api_http::shared::ci::jobs::CiReadError;
use codeyard_app::app::AppState;
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
