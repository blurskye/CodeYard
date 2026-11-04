use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Json;
use axum::Router;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_current_user;
use codeyard_app::app::AppState;
use std::sync::Arc;
use uuid::Uuid;
pub mod collection;
pub mod job;

pub(super) fn parse_job_id(job_id: &str) -> Result<Uuid, (StatusCode, Json<ApiError>)> {
    Uuid::parse_str(job_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "job_id must be a valid UUID",
            )),
        )
    })
}

async fn authorized_run(
    state: &AppState,
    headers: &HeaderMap,
    job_id: &str,
) -> Result<codeyard_db_core::models::CiRun, CiReadError> {
    let user = get_current_user(&state.pool, headers).await?;
    let run = codeyard_db_actions::ci::get_ci_run_by_run_id(&state.pool, parse_job_id(job_id)?)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    let repo =
        codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, run.repo_id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
    ensure_repo_read_access(state, &user, &repo).await?;
    Ok(run)
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/ci/jobs",
            get(crate::ci::jobs::collection::list_ci_jobs),
        )
        .route(
            "/api/ci/jobs/:job_id",
            get(crate::ci::jobs::job::get_ci_job),
        )
}

use codeyard_api_http::shared::ci::jobs::CiReadError;

use codeyard_api_http::shared::ci::jobs::ensure_repo_read_access;

use codeyard_api_http::shared::ci::jobs::missing;

use codeyard_api_http::shared::ci::jobs::unavailable;
