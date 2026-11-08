//! Browser session decoding and repository authority HTTP responses.
use crate::error::ApiError;
use crate::shared::repos::chrome::ApiResult;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use codeyard_database::models::Repository;
use codeyard_identity::repository_access::AccessError;
pub fn access_error(error: AccessError) -> (StatusCode, Json<ApiError>) {
    let (status, code, message) = match error {
        AccessError::NotFound => (StatusCode::NOT_FOUND, "not_found", "Repository not found"),
        AccessError::PasswordChangeRequired => (
            StatusCode::FORBIDDEN,
            "password_change_required",
            "Change your password before modifying repositories",
        ),
        AccessError::WriteDenied => (
            StatusCode::FORBIDDEN,
            "write_denied",
            "Archived repositories and password-change-required accounts cannot write",
        ),
        AccessError::InvalidCollaboratorOwner => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Cannot add repository owner as collaborator",
        ),
        AccessError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_error",
            "Repository authority is unavailable",
        ),
    };
    (status, Json(ApiError::new(code, message)))
}

pub async fn current_repository(
    pool: &codeyard_database::DbPool,
    id: i64,
) -> ApiResult<Repository> {
    codeyard_identity::repository_access::current_repository(pool, id)
        .await
        .map_err(|error| {
            if error == AccessError::Unavailable {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ApiError::new(
                        "database_error",
                        "Unable to check repository access",
                    )),
                )
            } else {
                access_error(error)
            }
        })
}

pub async fn check_repo_read_access(
    pool: &codeyard_database::DbPool,
    repository: &Repository,
    headers: &HeaderMap,
) -> ApiResult<()> {
    let repository = current_repository(pool, repository.id).await?;
    read_access(pool, &repository, headers).await
}

pub async fn read_access(
    pool: &codeyard_database::DbPool,
    repository: &Repository,
    headers: &HeaderMap,
) -> ApiResult<()> {
    // Public reads do not require browser authentication.
    if !repository.is_private {
        return Ok(());
    }
    let user = crate::session::get_optional_user(pool, headers).await?;
    if codeyard_identity::repository_access::can_read(pool, user.as_ref(), repository)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?
    {
        Ok(())
    } else {
        Err(access_error(AccessError::NotFound))
    }
}
