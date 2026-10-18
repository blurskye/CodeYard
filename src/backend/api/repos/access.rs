//! Browser session decoding and repository authority HTTP responses.
use crate::backend::{
    api::{http::error::ApiError, http::session},
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

type AccessResult<T> = Result<T, (StatusCode, Json<ApiError>)>;

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

async fn current_repository(pool: &db::DbPool, id: i64) -> AccessResult<Repository> {
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

pub(crate) async fn check_repo_read_access(
    pool: &db::DbPool,
    repository: &Repository,
    headers: &HeaderMap,
) -> AccessResult<()> {
    let repository = current_repository(pool, repository.id).await?;
    // Public reads do not require browser authentication.
    if !repository.is_private {
        return Ok(());
    }
    let user = session::get_optional_user(pool, headers).await?;
    if access::can_read(pool, user.as_ref(), &repository)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?
    {
        Ok(())
    } else {
        Err(access_error(AccessError::NotFound))
    }
}

pub(crate) async fn check_repo_admin_access(
    pool: &db::DbPool,
    repository: &Repository,
    headers: &HeaderMap,
) -> AccessResult<()> {
    let repository = current_repository(pool, repository.id).await?;
    let user = session::get_current_user(pool, headers).await?;
    if user.must_change_password {
        return Err(access_error(AccessError::PasswordChangeRequired));
    }
    if access::can_admin(pool, &user, &repository)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?
    {
        Ok(())
    } else {
        Err(access_error(AccessError::NotFound))
    }
}

pub(crate) async fn check_repo_write_access(
    pool: &db::DbPool,
    repository: &Repository,
    headers: &HeaderMap,
) -> AccessResult<User> {
    let repository = current_repository(pool, repository.id).await?;
    let user = session::get_current_user(pool, headers).await?;
    if access::can_write(pool, &user, &repository)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?
    {
        return Ok(user);
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
