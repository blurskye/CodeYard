use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{json_page, PageQuery};
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::pull_requests::reviews::CreatePullRequestReviewRequest;
use crate::backend::repositories::pull_requests::reviews::DismissPullRequestReviewRequest;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
pub async fn create_pull_request_review(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Json(req): Json<CreatePullRequestReviewRequest>,
) -> Result<(StatusCode, Json<PullRequestReviewResponse>), (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let review = crate::backend::repositories::pull_requests::reviews::create_pull_request_review(
        &state.pool,
        state.cluster.as_ref(),
        &state.notification_broadcaster,
        current_actor.id,
        (owner, repo, number),
        req,
    )
    .await
    .map_err(super::operation_error)?;
    let user = current_actor;

    Ok((
        StatusCode::CREATED,
        Json(PullRequestReviewResponse {
            id: review.id,
            pull_request_id: review.pull_request_id,
            author_id: review.author_id,
            author: Some(UserResponse::from(user)),
            state: review.state,
            body: review.body,
            commit_sha: review.commit_sha,
            dismissed_by_id: review.dismissed_by_id,
            dismissed_reason: review.dismissed_reason,
            dismissed_at: review.dismissed_at.map(|dt| dt.to_rfc3339()),
            created_at: review.created_at.to_rfc3339(),
        }),
    ))
}

pub async fn dismiss_pull_request_review(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number, review_id)): Path<(String, String, i32, i64)>,
    Json(req): Json<DismissPullRequestReviewRequest>,
) -> Result<Json<PullRequestReviewResponse>, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let dismissed =
        crate::backend::repositories::pull_requests::reviews::dismiss_pull_request_review(
            &state.pool,
            current_actor.id,
            owner,
            repo,
            number,
            review_id,
            req,
        )
        .await
        .map_err(super::operation_error)?;
    let author = db::users::accounts::get_user_by_id(&state.pool, dismissed.author_id)
        .await
        .map_err(crate::backend::api::http::error::discussion_write_error)?;

    Ok(Json(PullRequestReviewResponse {
        id: dismissed.id,
        pull_request_id: dismissed.pull_request_id,
        author_id: dismissed.author_id,
        author: author.map(UserResponse::from),
        state: dismissed.state,
        body: dismissed.body,
        commit_sha: dismissed.commit_sha,
        dismissed_by_id: dismissed.dismissed_by_id,
        dismissed_reason: dismissed.dismissed_reason,
        dismissed_at: dismissed.dismissed_at.map(|dt| dt.to_rfc3339()),
        created_at: dismissed.created_at.to_rfc3339(),
    }))
}

#[derive(Serialize)]
pub struct PullRequestReviewResponse {
    pub id: i64,
    pub pull_request_id: i64,
    pub author_id: i64,
    pub author: Option<UserResponse>,
    pub state: String,
    pub body: Option<String>,
    pub commit_sha: Option<String>,
    pub dismissed_by_id: Option<i64>,
    pub dismissed_reason: Option<String>,
    pub dismissed_at: Option<String>,
    pub created_at: String,
}
pub async fn list_pull_request_reviews(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<PullRequestReviewResponse>>, (StatusCode, Json<ApiError>)> {
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

    let reviews = db::pull_requests::reviews::list_pull_request_reviews(
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

    let mut responses = Vec::with_capacity(reviews.len());
    for entry in reviews {
        let author = Some(UserResponse::from_display(&entry.author));
        let review = entry.review;
        responses.push(PullRequestReviewResponse {
            id: review.id,
            pull_request_id: review.pull_request_id,
            author_id: review.author_id,
            author,
            state: review.state,
            body: review.body,
            commit_sha: review.commit_sha,
            dismissed_by_id: review.dismissed_by_id,
            dismissed_reason: review.dismissed_reason,
            dismissed_at: review.dismissed_at.map(|dt| dt.to_rfc3339()),
            created_at: review.created_at.to_rfc3339(),
        });
    }

    Ok(Json(responses))
}
