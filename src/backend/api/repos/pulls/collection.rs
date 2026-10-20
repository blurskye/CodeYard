use super::pull::{detail_response, PullRequestDetailResponse, PullRequestResponse};
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::check_repo_read_access;
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
    let user = current_actor;

    Ok((
        StatusCode::CREATED,
        Json(
            detail_response(
                &state.pool,
                pull_request,
                Some(PublicUserResponse::from(user)),
            )
            .await?,
        ),
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

    let (offset, limit) = crate::backend::api::repos::chrome::json_page(
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
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new(
                "database_error",
                "Collaboration storage is temporarily unavailable",
            )),
        )
    })?;

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
