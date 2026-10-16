use crate::backend::api::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{json_page, PageQuery};
use crate::backend::api::session::get_current_user;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::pull_requests::comments::CreatePullRequestCommentRequest;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
pub async fn create_pull_request_comment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Json(req): Json<CreatePullRequestCommentRequest>,
) -> Result<(StatusCode, Json<PullRequestCommentResponse>), (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let comment =
        crate::backend::repositories::pull_requests::comments::create_pull_request_comment(
            &state.pool,
            state.cluster.as_ref(),
            &state.notification_broadcaster,
            current_actor.id,
            owner,
            repo,
            number,
            req,
        )
        .await
        .map_err(super::operation_error)?;
    let user = current_actor;

    Ok((
        StatusCode::CREATED,
        Json(PullRequestCommentResponse {
            id: comment.id,
            pull_request_id: comment.pull_request_id,
            author_id: comment.author_id,
            author: Some(UserResponse::from(user)),
            body: comment.body,
            path: comment.path,
            line: comment.line,
            side: comment.side,
            commit_sha: comment.commit_sha,
            in_reply_to_comment_id: comment.in_reply_to_comment_id,
            resolved_at: comment.resolved_at.map(|at| at.to_rfc3339()),
            resolved_by_id: comment.resolved_by_id,
            created_at: comment.created_at.to_rfc3339(),
            updated_at: comment.updated_at.to_rfc3339(),
        }),
    ))
}

pub async fn resolve_pull_request_comment(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    path: Path<(String, String, i32, i64)>,
) -> Result<Json<PullRequestCommentResponse>, (StatusCode, Json<ApiError>)> {
    set_resolved(state, headers, path, true).await
}

pub async fn unresolve_pull_request_comment(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    path: Path<(String, String, i32, i64)>,
) -> Result<Json<PullRequestCommentResponse>, (StatusCode, Json<ApiError>)> {
    set_resolved(state, headers, path, false).await
}

async fn set_resolved(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number, comment_id)): Path<(String, String, i32, i64)>,
    resolved: bool,
) -> Result<Json<PullRequestCommentResponse>, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    let comment = crate::backend::repositories::pull_requests::comments::set_comment_resolved(
        &state.pool,
        actor.id,
        &owner,
        &repo,
        number,
        comment_id,
        resolved,
    )
    .await
    .map_err(super::operation_error)?;
    let author = db::users::accounts::get_user_by_id(&state.pool, comment.author_id)
        .await
        .ok()
        .flatten()
        .map(UserResponse::from);
    Ok(Json(PullRequestCommentResponse {
        id: comment.id,
        pull_request_id: comment.pull_request_id,
        author_id: comment.author_id,
        author,
        body: comment.body,
        path: comment.path,
        line: comment.line,
        side: comment.side,
        commit_sha: comment.commit_sha,
        in_reply_to_comment_id: comment.in_reply_to_comment_id,
        resolved_at: comment.resolved_at.map(|at| at.to_rfc3339()),
        resolved_by_id: comment.resolved_by_id,
        created_at: comment.created_at.to_rfc3339(),
        updated_at: comment.updated_at.to_rfc3339(),
    }))
}

#[derive(Serialize)]
pub struct PullRequestCommentResponse {
    pub id: i64,
    pub pull_request_id: i64,
    pub author_id: i64,
    pub author: Option<UserResponse>,
    pub body: String,
    pub path: Option<String>,
    pub line: Option<i32>,
    pub side: Option<String>,
    pub commit_sha: Option<String>,
    pub in_reply_to_comment_id: Option<i64>,
    pub resolved_at: Option<String>,
    pub resolved_by_id: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}
pub async fn list_pull_request_comments(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<PullRequestCommentResponse>>, (StatusCode, Json<ApiError>)> {
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
    let (offset, limit) = json_page(query.page, query.per_page)?;

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

    let comments = db::pull_requests::reviews::list_pull_request_comments(
        &state.pool,
        pull_request.id,
        limit as i64,
        offset as i64,
    )
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

    let mut responses = Vec::with_capacity(comments.len());
    for entry in comments {
        let author = entry.author.as_ref().map(UserResponse::from_display);
        let comment = entry.comment;
        responses.push(PullRequestCommentResponse {
            id: comment.id,
            pull_request_id: comment.pull_request_id,
            author_id: comment.author_id,
            author,
            body: comment.body,
            path: comment.path,
            line: comment.line,
            side: comment.side,
            commit_sha: comment.commit_sha,
            in_reply_to_comment_id: comment.in_reply_to_comment_id,
            resolved_at: comment.resolved_at.map(|at| at.to_rfc3339()),
            resolved_by_id: comment.resolved_by_id,
            created_at: comment.created_at.to_rfc3339(),
            updated_at: comment.updated_at.to_rfc3339(),
        });
    }

    Ok(Json(responses))
}
