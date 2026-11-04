use crate::access::readable_repository;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_optional_user;
use codeyard_api_http::shared::repos::access::access_error;
use codeyard_app::app::AppState;
use codeyard_identity::repository_access::AccessError;
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
pub struct RepoPermissionsResponse {
    pub can_read: bool,
    pub can_write: bool,
    pub can_admin: bool,
    pub role: String,
}

pub async fn get_repo_permissions(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<RepoPermissionsResponse>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    let current_user = get_optional_user(&state.pool, &headers).await?;

    // Write and admin come from the same policy the write paths use, so an archived repository
    // is not reported as writable.
    let (can_write, can_admin, role) = if let Some(user) = current_user {
        let unavailable = || access_error(AccessError::Unavailable);
        let can_write =
            codeyard_identity::repository_access::can_write(&state.pool, &user, &repository)
                .await
                .map_err(|_| unavailable())?;
        let can_admin =
            codeyard_identity::repository_access::can_admin(&state.pool, &user, &repository)
                .await
                .map_err(|_| unavailable())?;
        let role = if user.is_admin && repository.owner_kind == "user" {
            "admin"
        } else if repository.is_personal_owner(user.id) {
            "owner"
        } else {
            codeyard_db_repos::repositories::access::get_user_repo_access(
                &state.pool,
                user.id,
                repository.id,
            )
            .await
            .map_err(|_| unavailable())?
            .as_str()
        };
        (can_write, can_admin, role.to_string())
    } else {
        (false, false, "anonymous".to_string())
    };

    Ok(Json(RepoPermissionsResponse {
        can_read: true,
        can_write,
        can_admin,
        role,
    }))
}
