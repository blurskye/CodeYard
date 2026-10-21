//! The pull request JSON answers.
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::readable_repository;
use crate::backend::api::repos::discussion::ApiFailure;
use crate::backend::api::repos::responses::storage_unavailable;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::pull_requests::mutations::UpdatePullRequestRequest;
use crate::backend::repositories::pull_requests::stacks;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;

pub async fn update_pull_request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Json(req): Json<UpdatePullRequestRequest>,
) -> Result<Json<PullRequestDetailResponse>, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let pull_request = crate::backend::repositories::pull_requests::mutations::update_pull_request(
        &state.pool,
        &state.ci_runs,
        &state.notification_broadcaster,
        current_actor.id,
        (owner, repo, number),
        req,
    )
    .await
    .map_err(super::operation_error)?;
    Ok(Json(detail_response(&state.pool, pull_request).await?))
}

#[derive(Serialize)]
pub struct PullRequestResponse {
    pub id: i64,
    pub number: i32,
    pub title: String,
    pub body: String,
    pub state: String,
    pub head_repo_id: Option<i64>,
    pub head_branch: String,
    pub head_sha: String,
    pub base_branch: String,
    pub base_sha: String,
    pub is_draft: bool,
    pub mergeable: Option<bool>,
    pub stack_id: Option<i64>,
    pub author_id: i64,
    pub author: Option<UserResponse>,
    pub created_at: String,
    pub updated_at: String,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
}

impl PullRequestResponse {
    pub fn from_pull_request(
        pull_request: db::models::PullRequest,
        author: Option<UserResponse>,
    ) -> Self {
        Self {
            id: pull_request.id,
            number: pull_request.number,
            title: pull_request.title,
            body: pull_request.body,
            state: pull_request.state,
            head_repo_id: pull_request.head_repo_id,
            head_branch: pull_request.head_branch,
            head_sha: pull_request.head_sha,
            base_branch: pull_request.base_branch,
            base_sha: pull_request.base_sha,
            is_draft: pull_request.is_draft,
            mergeable: pull_request.mergeable,
            stack_id: pull_request.stack_id,
            author_id: pull_request.author_id,
            author,
            created_at: pull_request.created_at.to_rfc3339(),
            updated_at: pull_request.updated_at.to_rfc3339(),
            merged_at: pull_request.merged_at.map(|dt| dt.to_rfc3339()),
            closed_at: pull_request.closed_at.map(|dt| dt.to_rfc3339()),
        }
    }
}

/// A single pull request answer: the pull request, and the stack it is in when it is in one.
#[derive(Serialize)]
pub struct PullRequestDetailResponse {
    #[serde(flatten)]
    pub pull_request: PullRequestResponse,
    pub stack: Option<StackResponse>,
}

#[derive(Serialize)]
pub struct StackResponse {
    pub id: i64,
    pub base_branch: String,
    pub size: usize,
    pub position: usize,
    pub entries: Vec<StackEntryResponse>,
}

#[derive(Serialize)]
pub struct StackEntryResponse {
    pub number: i32,
    pub title: String,
    pub state: String,
    pub merged: bool,
    pub draft: bool,
    pub head_branch: String,
    pub base_branch: String,
}

/// The single pull request answer. A storage failure is an error, never an answer without
/// the author or the stack.
pub(crate) async fn detail_response(
    pool: &db::DbPool,
    pull_request: db::models::PullRequest,
) -> Result<PullRequestDetailResponse, ApiFailure> {
    let author = db::users::accounts::get_user_by_id(pool, pull_request.author_id)
        .await
        .map_err(|_| storage_unavailable())?;
    let stack = stacks::stack_of(pool, &pull_request)
        .await
        .map_err(|_| storage_unavailable())?;
    let stack = stack.map(|stack| {
        let position = stack.position(pull_request.id);
        let base_branch = stack.trunk().to_owned();
        StackResponse {
            id: stack.id,
            base_branch,
            size: stack.layers.len(),
            position,
            entries: stack
                .layers
                .into_iter()
                .map(|layer| StackEntryResponse {
                    number: layer.number,
                    title: layer.title,
                    state: layer.state,
                    merged: layer.merged,
                    draft: layer.is_draft,
                    head_branch: layer.head_branch,
                    base_branch: layer.base_branch,
                })
                .collect(),
        }
    });
    Ok(PullRequestDetailResponse {
        pull_request: PullRequestResponse::from_pull_request(
            pull_request,
            author.map(UserResponse::from),
        ),
        stack,
    })
}

pub async fn get_pull_request(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
) -> Result<Json<PullRequestDetailResponse>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;

    let pull_request =
        db::pull_requests::records::get_pull_request(&state.pool, repository.id, number)
            .await
            .map_err(|_| storage_unavailable())?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ApiError::new("not_found", "Pull request not found")),
                )
            })?;

    Ok(Json(detail_response(&state.pool, pull_request).await?))
}
