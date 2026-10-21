use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::readable_repository;
use crate::backend::api::repos::paging::{json_page, PageQuery};
use crate::backend::api::repos::responses::storage_unavailable;
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

    Ok((
        StatusCode::CREATED,
        Json(PullRequestReviewResponse::new(
            review,
            Some(UserResponse::from(current_actor)),
        )),
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

    Ok(Json(PullRequestReviewResponse::new(
        dismissed,
        author.map(UserResponse::from),
    )))
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

impl PullRequestReviewResponse {
    fn new(review: db::models::PullRequestReview, author: Option<UserResponse>) -> Self {
        Self {
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
        }
    }
}

pub async fn list_pull_request_reviews(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<PullRequestReviewResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    let (offset, limit) = json_page(query.page, query.per_page)?;

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

    let reviews = db::pull_requests::reviews::list_pull_request_reviews(
        &state.pool,
        pull_request.id,
        limit as i64,
        offset as i64,
    )
    .await
    .map_err(|_| storage_unavailable())?;

    let mut responses = Vec::with_capacity(reviews.len());
    for entry in reviews {
        let author = Some(UserResponse::from_display(&entry.author));
        responses.push(PullRequestReviewResponse::new(entry.review, author));
    }

    Ok(Json(responses))
}
