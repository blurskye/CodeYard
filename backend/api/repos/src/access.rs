use axum::http::HeaderMap;
use codeyard_api_http::shared::repos::chrome::ApiResult;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::User;
use codeyard_identity::repository_access::AccessError;

/// The repository at `owner/repo`: 404 when there is none, 503 when the lookup fails.
pub(crate) async fn find_repository(
    pool: &codeyard_db_core::DbPool,
    owner: &str,
    repo: &str,
) -> ApiResult<Repository> {
    codeyard_db_repos::repositories::records::get_repository(pool, owner, repo)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?
        .ok_or_else(|| access_error(AccessError::NotFound))
}

/// The repository at `owner/repo`, once the caller is known to be allowed to read it.
pub(crate) async fn readable_repository(
    pool: &codeyard_db_core::DbPool,
    owner: &str,
    repo: &str,
    headers: &HeaderMap,
) -> ApiResult<Repository> {
    let repository = find_repository(pool, owner, repo).await?;
    read_access(pool, &repository, headers).await?;
    Ok(repository)
}

/// Checks that `user` administers the repository.
pub(crate) async fn check_repo_admin_access(
    pool: &codeyard_db_core::DbPool,
    repository: &Repository,
    user: &User,
) -> ApiResult<()> {
    let repository = current_repository(pool, repository.id).await?;
    if user.must_change_password {
        return Err(access_error(AccessError::PasswordChangeRequired));
    }
    if codeyard_identity::repository_access::can_admin(pool, user, &repository)
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
    pool: &codeyard_db_core::DbPool,
    repository: &Repository,
    user: &User,
) -> ApiResult<()> {
    let repository = current_repository(pool, repository.id).await?;
    if codeyard_identity::repository_access::can_write(pool, user, &repository)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?
    {
        return Ok(());
    }
    if !codeyard_identity::repository_access::can_read(pool, Some(user), &repository)
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
    use axum::http::StatusCode;
    use axum::Json;

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

use codeyard_api_http::shared::repos::access::access_error;

use codeyard_api_http::shared::repos::access::current_repository;

use codeyard_api_http::shared::repos::access::read_access;
