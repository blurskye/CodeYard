use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_optional_user;
use crate::backend::api::repos::access::{access_error, readable_repository};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::access::{self, AccessError};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
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
        let can_write = access::can_write(&state.pool, &user, &repository)
            .await
            .map_err(|_| unavailable())?;
        let can_admin = access::can_admin(&state.pool, &user, &repository)
            .await
            .map_err(|_| unavailable())?;
        let role = if user.is_admin && repository.owner_kind == "user" {
            "admin"
        } else if repository.is_personal_owner(user.id) {
            "owner"
        } else {
            db::repositories::access::get_user_repo_access(&state.pool, user.id, repository.id)
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
