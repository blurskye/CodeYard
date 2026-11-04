use crate::access::check_repo_admin_access;
use crate::access::find_repository;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_current_user;
use codeyard_api_http::shared::repos::responses::repository_unavailable;
use codeyard_app::app::AppState;
use serde::Deserialize;
use serde::Serialize;
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

    let collaborators = codeyard_db_repos::repositories::access::list_repo_collaborators(
        &state.pool,
        repository.id,
    )
    .await
    .map_err(|_| repository_unavailable())?;

    let user_ids: Vec<i64> = collaborators.iter().map(|access| access.user_id).collect();
    let users = codeyard_db_identity::users::accounts::get_users_by_ids(&state.pool, &user_ids)
        .await
        .map_err(|_| repository_unavailable())?;
    let user_map: std::collections::HashMap<i64, codeyard_db_core::models::User> =
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
    let access_level = match req.access_level.as_deref() {
        None => codeyard_db_core::models::AccessLevel::Read,
        Some(name) => {
            codeyard_db_core::models::AccessLevel::parse_collaborator(name).ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(ApiError::new(
                        "validation_error",
                        "Choose read, write or admin collaborator access.",
                    )),
                )
            })?
        }
    };
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    let target_user = codeyard_identity::repository_access::change_collaborator(
        &state.pool,
        codeyard_db_core::queries::credentials::Actor::from((
            user.id,
            codeyard_api_http::session::extract_session_id(&headers).unwrap_or_default(),
        )),
        repository.id,
        &req.username,
        Some(access_level),
    )
    .await
    .map_err(codeyard_api_http::shared::repos::access::access_error)?;

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

    codeyard_identity::repository_access::change_collaborator(
        &state.pool,
        codeyard_db_core::queries::credentials::Actor::from((
            user.id,
            codeyard_api_http::session::extract_session_id(&headers).unwrap_or_default(),
        )),
        repository.id,
        &username,
        None,
    )
    .await
    .map_err(codeyard_api_http::shared::repos::access::access_error)?;

    Ok(StatusCode::NO_CONTENT)
}
