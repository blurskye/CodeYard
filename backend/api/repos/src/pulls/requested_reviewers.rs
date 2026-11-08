use crate::access::readable_repository;
use crate::responses::storage_unavailable;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_current_user;
use codeyard_app::app::AppState;
use codeyard_repos_pulls::repositories::pull_requests::reviews::RequestReviewerRequest;
use serde::Serialize;
use std::sync::Arc;

pub async fn request_reviewer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Json(req): Json<RequestReviewerRequest>,
) -> Result<(StatusCode, Json<RequestedReviewer>), (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let result = codeyard_repos_pulls::repositories::pull_requests::reviews::request_reviewer(
        &state.pool,
        &state.notification_broadcaster,
        current_actor.id,
        owner,
        repo,
        number,
        req,
    )
    .await
    .map_err(crate::pulls::operation_error)?;

    let status = if result.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(match result.target {
            codeyard_repos_pulls::repositories::pull_requests::reviews::ReviewerTarget::User(
                user,
            ) => RequestedReviewer::User(RequestedReviewerResponse::new(user)),
            codeyard_repos_pulls::repositories::pull_requests::reviews::ReviewerTarget::Team(
                team,
            ) => RequestedReviewer::Team(team),
        }),
    ))
}

pub async fn remove_requested_reviewer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number, username)): Path<(String, String, i32, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    codeyard_repos_pulls::repositories::pull_requests::reviews::remove_requested_reviewer(
        &state.pool,
        current_actor.id,
        owner,
        repo,
        number,
        username,
    )
    .await
    .map_err(crate::pulls::operation_error)?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum RequestedReviewer {
    User(RequestedReviewerResponse),
    Team(codeyard_database::models::Team),
}

#[derive(Serialize)]
pub struct RequestedReviewerResponse {
    pub user_id: i64,
    pub username: String,
    pub avatar_url: Option<String>,
}

impl RequestedReviewerResponse {
    fn new(user: codeyard_database::models::User) -> Self {
        Self {
            user_id: user.id,
            username: user.username,
            avatar_url: user
                .avatar_file_id
                .map(|id| format!("/api/files/{id}/download")),
        }
    }
}

pub async fn list_requested_reviewers(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
) -> Result<Json<Vec<RequestedReviewer>>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    let pull_request = codeyard_db_repos::pull_requests::records::get_pull_request(
        &state.pool,
        repository.id,
        number,
    )
    .await
    .map_err(|_| storage_unavailable())?
    .ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Pull request not found")),
        )
    })?;

    let users = codeyard_db_repos::pull_requests::reviews::list_pull_request_reviewers(
        &state.pool,
        pull_request.id,
    )
    .await
    .map_err(|_| storage_unavailable())?;

    let viewer = codeyard_api_http::session::get_optional_user(&state.pool, &headers).await?;
    let teams = codeyard_db_repos::pull_requests::team_reviewers::list(
        &state.pool,
        repository.id,
        Some(pull_request.id),
        viewer.as_ref().map(|user| user.id),
    )
    .await
    .map_err(|_| storage_unavailable())?;
    let mut requested: Vec<_> = users
        .into_iter()
        .map(|user| RequestedReviewer::User(RequestedReviewerResponse::new(user)))
        .collect();
    requested.extend(
        teams
            .into_iter()
            .map(|access| RequestedReviewer::Team(access.team)),
    );
    Ok(Json(requested))
}
