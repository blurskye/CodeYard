//! Browser session decoding and repository authority HTTP responses.
use crate::backend::{
    api::{http::error::ApiError, http::session, repos::chrome::ApiResult},
    db::{
        self,
        models::{Repository, User},
    },
    repositories::access::{self, AccessError},
};
use axum::{
    http::{HeaderMap, StatusCode},
    Json,
};

pub(crate) fn access_error(error: AccessError) -> (StatusCode, Json<ApiError>) {
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

async fn current_repository(pool: &db::DbPool, id: i64) -> ApiResult<Repository> {
    access::current_repository(pool, id).await.map_err(|error| {
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

/// The repository at `owner/repo`: 404 when there is none, 503 when the lookup fails.
pub(crate) async fn find_repository(
    pool: &db::DbPool,
    owner: &str,
    repo: &str,
) -> ApiResult<Repository> {
    db::repositories::records::get_repository(pool, owner, repo)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?
        .ok_or_else(|| access_error(AccessError::NotFound))
}

/// The repository at `owner/repo`, once the caller is known to be allowed to read it.
pub(crate) async fn readable_repository(
    pool: &db::DbPool,
    owner: &str,
    repo: &str,
    headers: &HeaderMap,
) -> ApiResult<Repository> {
    let repository = find_repository(pool, owner, repo).await?;
    read_access(pool, &repository, headers).await?;
    Ok(repository)
}

pub(crate) async fn check_repo_read_access(
    pool: &db::DbPool,
    repository: &Repository,
    headers: &HeaderMap,
) -> ApiResult<()> {
    let repository = current_repository(pool, repository.id).await?;
    read_access(pool, &repository, headers).await
}

async fn read_access(
    pool: &db::DbPool,
    repository: &Repository,
    headers: &HeaderMap,
) -> ApiResult<()> {
    // Public reads do not require browser authentication.
    if !repository.is_private {
        return Ok(());
    }
    let user = session::get_optional_user(pool, headers).await?;
    if access::can_read(pool, user.as_ref(), repository)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?
    {
        Ok(())
    } else {
        Err(access_error(AccessError::NotFound))
    }
}

/// Checks that `user` administers the repository.
pub(crate) async fn check_repo_admin_access(
    pool: &db::DbPool,
    repository: &Repository,
    user: &User,
) -> ApiResult<()> {
    let repository = current_repository(pool, repository.id).await?;
    if user.must_change_password {
        return Err(access_error(AccessError::PasswordChangeRequired));
    }
    if access::can_admin(pool, user, &repository)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?
    {
        Ok(())
    } else {
        Err(access_error(AccessError::NotFound))
    }
}

/// Checks that `user` may write to the repository.
pub(crate) async fn check_repo_write_access(
    pool: &db::DbPool,
    repository: &Repository,
    user: &User,
) -> ApiResult<()> {
    let repository = current_repository(pool, repository.id).await?;
    if access::can_write(pool, user, &repository)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?
    {
        return Ok(());
    }
    if !access::can_read(pool, Some(user), &repository)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?
    {
        return Err(access_error(AccessError::NotFound));
    }
    if repository.is_archived || user.must_change_password {
        return Err(access_error(AccessError::WriteDenied));
    }
    Err(access_error(AccessError::NotFound))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_access_failures_preserve_status_and_json_contracts() {
        for (error, expected_status, expected_code) in [
            (AccessError::NotFound, StatusCode::NOT_FOUND, "not_found"),
            (
                AccessError::PasswordChangeRequired,
                StatusCode::FORBIDDEN,
                "password_change_required",
            ),
            (
                AccessError::WriteDenied,
                StatusCode::FORBIDDEN,
                "write_denied",
            ),
            (
                AccessError::InvalidCollaboratorOwner,
                StatusCode::BAD_REQUEST,
                "validation_error",
            ),
            (
                AccessError::Unavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "database_error",
            ),
        ] {
            let (status, Json(body)) = access_error(error);
            assert_eq!(status, expected_status);
            assert_eq!(body.error, expected_code);
            assert_eq!(
                serde_json::to_value(body)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .len(),
                2
            );
        }
    }
}
