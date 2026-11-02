use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::{check_repo_admin_access, find_repository};
use crate::backend::api::repos::responses::repository_unavailable;
use crate::backend::app::AppState;
use crate::backend::db;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Serialize)]
pub struct CollaboratorResponse {
    pub user_id: i64,
    pub username: String,
    pub avatar_url: Option<String>,
    pub access_level: String,
}

#[derive(Deserialize)]
pub struct AddCollaboratorRequest {
    pub username: String,
    pub access_level: Option<String>,
}

pub async fn list_collaborators(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<Vec<CollaboratorResponse>>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &user).await?;
    // Collaborator management remains personal-owner-only, not a new org/team policy.
    if !repository.is_personal_owner(user.id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Repository not found")),
        ));
    }

    let collaborators =
        db::repositories::access::list_repo_collaborators(&state.pool, repository.id)
            .await
            .map_err(|_| repository_unavailable())?;

    let user_ids: Vec<i64> = collaborators.iter().map(|access| access.user_id).collect();
    let users = db::users::accounts::get_users_by_ids(&state.pool, &user_ids)
        .await
        .map_err(|_| repository_unavailable())?;
    let user_map: std::collections::HashMap<i64, db::models::User> =
        users.into_iter().map(|user| (user.id, user)).collect();

    Ok(Json(
        collaborators
            .into_iter()
            .filter_map(|access| {
                let user = user_map.get(&access.user_id)?;
                Some(CollaboratorResponse {
                    user_id: user.id,
                    username: user.username.clone(),
                    avatar_url: user
                        .avatar_file_id
                        .map(|id| format!("/api/files/{}/download", id)),
                    access_level: access.level().as_str().to_string(),
                })
            })
            .collect(),
    ))
}

pub async fn add_collaborator(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<AddCollaboratorRequest>,
) -> Result<Json<CollaboratorResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;

    let access_level = match req.access_level.as_deref() {
        None => db::models::AccessLevel::Read,
        Some(name) => db::models::AccessLevel::parse_collaborator(name).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(
                    "validation_error",
                    "Choose read, write or admin collaborator access.",
                )),
            )
        })?,
    };
    let target_user = crate::backend::repositories::access::change_collaborator(
        &state.pool,
        user.id,
        repository.id,
        &req.username,
        Some(access_level),
    )
    .await
    .map_err(crate::backend::api::repos::access::access_error)?;

    Ok(Json(CollaboratorResponse {
        user_id: target_user.id,
        username: target_user.username,
        avatar_url: target_user
            .avatar_file_id
            .map(|id| format!("/api/files/{}/download", id)),
        access_level: access_level.as_str().to_string(),
    }))
}

pub async fn remove_collaborator(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, username)): Path<(String, String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;

    crate::backend::repositories::access::change_collaborator(
        &state.pool,
        user.id,
        repository.id,
        &username,
        None,
    )
    .await
    .map_err(crate::backend::api::repos::access::access_error)?;

    Ok(StatusCode::NO_CONTENT)
}
