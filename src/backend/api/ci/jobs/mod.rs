pub mod collection;
pub mod job;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::app::AppState;

use axum::{
    http::{HeaderMap, StatusCode},
    routing::get,
    Json, Router,
};
use std::sync::Arc;
use uuid::Uuid;

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
type CiReadError = (StatusCode, Json<ApiError>);
fn unavailable(_: anyhow::Error) -> CiReadError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError::new(
            "database_error",
            "CI history or authority is unavailable",
        )),
    )
}
fn missing() -> CiReadError {
    (
        StatusCode::NOT_FOUND,
        Json(ApiError::new("not_found", "CI job or repository not found")),
    )
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

pub(super) async fn ensure_repo_read_access(
    state: &AppState,
    user: &codeyard_db_core::models::User,
    repository: &codeyard_db_core::models::Repository,
) -> Result<(), CiReadError> {
    if crate::backend::repositories::access::can_read(&state.pool, Some(user), repository)
        .await
        .map_err(unavailable)?
    {
        Ok(())
    } else {
        Err(missing())
    }
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/ci/jobs", get(collection::list_ci_jobs))
        .route("/api/ci/jobs/:job_id", get(job::get_ci_job))
}
