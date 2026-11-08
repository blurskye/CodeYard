use crate::error::ApiError;
use axum::http::StatusCode;
use axum::Json;
use codeyard_app::app::AppState;
pub type CiReadError = (StatusCode, Json<ApiError>);

pub fn unavailable(_: anyhow::Error) -> CiReadError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError::new(
            "database_error",
            "CI history or authority is unavailable",
        )),
    )
}

pub fn missing() -> CiReadError {
    (
        StatusCode::NOT_FOUND,
        Json(ApiError::new("not_found", "CI job or repository not found")),
    )
}

pub async fn ensure_repo_read_access(
    state: &AppState,
    user: &codeyard_database::models::User,
    repository: &codeyard_database::models::Repository,
) -> Result<(), CiReadError> {
    if codeyard_identity::repository_access::can_read(&state.pool, Some(user), repository)
        .await
        .map_err(unavailable)?
    {
        Ok(())
    } else {
        Err(missing())
    }
}
