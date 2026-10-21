use super::pull::{detail_response, PullRequestDetailResponse, PullRequestResponse};
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::readable_repository;
use crate::backend::api::repos::responses::storage_unavailable;
use crate::backend::api::user::responses::PublicUserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::pull_requests::mutations::CreatePullRequestRequest;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;
pub async fn create_pull_request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<CreatePullRequestRequest>,
) -> Result<(StatusCode, Json<PullRequestDetailResponse>), (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let pull_request = crate::backend::repositories::pull_requests::mutations::create_pull_request(
        &state.pool,
        &state.ci_runs,
        &state.notification_broadcaster,
        current_actor.id,
        owner,
        repo,
        req,
    )
    .await
    .map_err(super::operation_error)?;

    Ok((
        StatusCode::CREATED,
        Json(detail_response(&state.pool, pull_request).await?),
    ))
}

#[derive(Deserialize)]
pub struct PullRequestQuery {
    pub state: Option<String>,
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}
pub async fn list_pull_requests(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<PullRequestQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<PullRequestResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;

    let pr_state = match query.state.as_deref() {
        Some("open") | Some("closed") => query.state.as_deref(),
        Some("all") | None => None,
        Some(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(
                    "validation_error",
                    "state must be one of: open, closed, all",
                )),
            ))
        }
    };

    let (offset, limit) = crate::backend::api::repos::paging::json_page(
        query.page,
        Some(query.per_page.unwrap_or(100)),
    )?;
    let prs = db::pull_requests::records::list_repo_pull_requests(
        &state.pool,
        repository.id,
        &db::discussions::ListFilter {
            state: pr_state,
            include_merged: true,
            offset: offset as i64,
            ..Default::default()
        },
        limit as i64,
    )
    .await
    .map_err(|_| storage_unavailable())?;

    let mut responses = Vec::with_capacity(prs.len());
    for entry in prs {
        let author = Some(PublicUserResponse::from_display(&entry.author));
        responses.push(PullRequestResponse::from_pull_request(
            entry.pull_request,
            author,
        ));
    }

    Ok(Json(responses))
}
