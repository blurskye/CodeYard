use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::{check_repo_read_access, check_repo_write_access};
use crate::backend::api::repos::responses::map_commit_status_response;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Serialize)]
pub struct CommitStatusResponse {
    pub id: i64,
    pub sha: String,
    pub context: String,
    pub state: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub creator_id: Option<i64>,
    pub creator: Option<UserResponse>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Deserialize)]
pub struct CreateCommitStatusRequest {
    pub state: String,
    pub context: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
}

pub(super) fn is_valid_sha1_hex(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_valid_commit_status_state(state: &str) -> bool {
    matches!(state, "pending" | "success" | "failure" | "error")
}

pub async fn list_commit_statuses(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<CommitStatusResponse>>, (StatusCode, Json<ApiError>)> {
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
    if !is_valid_sha1_hex(&sha) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "sha must be a full 40-character hexadecimal commit id",
            )),
        ));
    }
    let sha = sha.to_ascii_lowercase();

    let statuses =
        db::repositories::protection::list_commit_statuses(&state.pool, repository.id, &sha)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError::new(
                        "database_error",
                        "Repository operation is unavailable",
                    )),
                )
            })?;

    let mut responses = Vec::with_capacity(statuses.len());
    for status in statuses {
        let creator = if let Some(creator_id) = status.creator_id {
            db::users::accounts::get_user_by_id(&state.pool, creator_id)
                .await
                .ok()
                .flatten()
                .map(UserResponse::from)
        } else {
            None
        };
        responses.push(map_commit_status_response(status, creator));
    }

    Ok(Json(responses))
}

pub async fn create_commit_status(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(req): Json<CreateCommitStatusRequest>,
) -> Result<(StatusCode, Json<CommitStatusResponse>), (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
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

    check_repo_write_access(&state.pool, &repository, &headers).await?;
    if !is_valid_sha1_hex(&sha) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "sha must be a full 40-character hexadecimal commit id",
            )),
        ));
    }
    let sha = sha.to_ascii_lowercase();

    let state_value = req.state.trim().to_lowercase();
    if !is_valid_commit_status_state(&state_value) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "state must be one of: pending, success, failure, error",
            )),
        ));
    }

    let context = req.context.trim();
    if db::repositories::protection::server_owned_status_context(context) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiError::new(
                "reserved_context",
                "This status context is managed by the Actions executor",
            )),
        ));
    }
    if context.is_empty() || context.len() > 255 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "context must be 1-255 characters",
            )),
        ));
    }

    let description = req
        .description
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let target_url = req
        .target_url
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let status = db::repositories::protection::upsert_commit_status(
        &state.pool,
        db::repositories::protection::CommitStatusUpsert {
            repo_id: repository.id,
            sha: &sha,
            context,
            state: &state_value,
            description,
            target_url,
            creator_id: Some(user.id),
        },
    )
    .await
    .map_err(crate::backend::api::http::error::protection_write_error)?;

    Ok((
        StatusCode::CREATED,
        Json(map_commit_status_response(
            status,
            Some(UserResponse::from(user)),
        )),
    ))
}
