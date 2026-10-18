use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_optional_user;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::app::AppState;
use crate::backend::db;
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
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Repository operation is unavailable",
                )),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_read_access(&state.pool, &repository, &headers).await?;
    let current_user = get_optional_user(&state.pool, &headers).await?;

    let (can_write, can_admin, role) = if let Some(user) = current_user {
        // Write permission comes from the same policy the write paths use, so an
        // archived repository is not reported as writable.
        let unavailable = || {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new(
                    "database_error",
                    "Repository authority is unavailable",
                )),
            )
        };
        let can_write =
            crate::backend::repositories::access::can_write(&state.pool, &user, &repository)
                .await
                .map_err(|_| unavailable())?;
        if user.is_admin && repository.owner_kind == "user" {
            (can_write, true, "admin".to_string())
        } else if repository.is_personal_owner(user.id) {
            (can_write, true, "owner".to_string())
        } else {
            let access =
                db::repositories::access::get_user_repo_access(&state.pool, user.id, repository.id)
                    .await
                    .map_err(|_| unavailable())?;

            let role = match access {
                db::models::AccessLevel::Owner => "owner",
                db::models::AccessLevel::Admin => "admin",
                db::models::AccessLevel::Maintain => "maintain",
                db::models::AccessLevel::Write => "write",
                db::models::AccessLevel::Triage => "triage",
                db::models::AccessLevel::Read => "read",
                db::models::AccessLevel::None => "none",
            }
            .to_string();

            (can_write, access.can_admin(), role)
        }
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
