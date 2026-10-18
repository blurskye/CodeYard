use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::pull_requests::reviews::RequestReviewerRequest;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
pub async fn request_reviewer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Json(req): Json<RequestReviewerRequest>,
) -> Result<(StatusCode, Json<RequestedReviewerResponse>), (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let result = crate::backend::repositories::pull_requests::reviews::request_reviewer(
        &state.pool,
        &state.notification_broadcaster,
        current_actor.id,
        owner,
        repo,
        number,
        req,
    )
    .await
    .map_err(super::operation_error)?;

    let status = if result.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(RequestedReviewerResponse {
            user_id: result.target.id,
            username: result.target.username,
            avatar_url: result
                .target
                .avatar_file_id
                .map(|id| format!("/api/files/{}/download", id)),
        }),
    ))
}

pub async fn remove_requested_reviewer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number, username)): Path<(String, String, i32, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    crate::backend::repositories::pull_requests::reviews::remove_requested_reviewer(
        &state.pool,
        current_actor.id,
        owner,
        repo,
        number,
        username,
    )
    .await
    .map_err(super::operation_error)?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct RequestedReviewerResponse {
    pub user_id: i64,
    pub username: String,
    pub avatar_url: Option<String>,
}
pub async fn list_requested_reviewers(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
) -> Result<Json<Vec<RequestedReviewerResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Collaboration storage is temporarily unavailable",
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
    let pull_request =
        db::pull_requests::records::get_pull_request(&state.pool, repository.id, number)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError::new(
                        "database_error",
                        "Collaboration storage is temporarily unavailable",
                    )),
                )
            })?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ApiError::new("not_found", "Pull request not found")),
                )
            })?;

    let users =
        db::pull_requests::reviews::list_pull_request_reviewers(&state.pool, pull_request.id)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError::new(
                        "database_error",
                        "Collaboration storage is temporarily unavailable",
                    )),
                )
            })?;

    Ok(Json(
        users
            .into_iter()
            .map(|user| RequestedReviewerResponse {
                user_id: user.id,
                username: user.username,
                avatar_url: user
                    .avatar_file_id
                    .map(|id| format!("/api/files/{}/download", id)),
            })
            .collect(),
    ))
}
