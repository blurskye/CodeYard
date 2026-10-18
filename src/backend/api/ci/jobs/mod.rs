pub mod cancel;
pub mod collection;
pub mod deployment;
pub mod history;
pub mod job;
pub mod logs;
use crate::backend::{
    api::{http::error::ApiError, http::session::get_current_user},
    app::AppState,
    db,
};
use axum::{
    http::{HeaderMap, StatusCode},
    routing::{get, post},
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
) -> Result<db::models::CiRun, CiReadError> {
    let user = get_current_user(&state.pool, headers).await?;
    let run = db::ci::get_ci_run_by_run_id(&state.pool, parse_job_id(job_id)?)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    let repo = db::repositories::records::get_repository_by_id(&state.pool, run.repo_id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    ensure_repo_read_access(state, &user, &repo).await?;
    Ok(run)
}

pub(super) async fn ensure_repo_read_access(
    state: &AppState,
    user: &db::models::User,
    repository: &db::models::Repository,
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
        .route("/api/ci/jobs/:job_id/logs", get(logs::get_ci_job_logs))
        .route(
            "/api/ci/jobs/:job_id/history",
            get(history::get_ci_job_history),
        )
        .route(
            "/api/ci/jobs/:job_id/log-download",
            get(logs::download_ci_job_logs),
        )
        .route("/api/ci/jobs/:job_id/cancel", post(cancel::cancel_ci_job))
        .route(
            "/api/ci/jobs/:job_id/deployment",
            get(deployment::get_ci_deployment),
        )
}
