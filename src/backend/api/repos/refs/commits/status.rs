use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{json_page, PageQuery};
use crate::backend::api::repos::refs::commits::statuses::{
    is_valid_sha1_hex, status_responses, CommitStatusResponse,
};
use crate::backend::app::AppState;
use crate::backend::db;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
#[derive(Serialize)]
pub struct CombinedCommitStatusResponse {
    pub sha: String,
    pub state: String,
    pub total_count: usize,
    pub statuses: Vec<CommitStatusResponse>,
}

fn combined_commit_status_state(statuses: &[(String, i64)]) -> String {
    if statuses
        .iter()
        .any(|(state, count)| *count > 0 && (state == "failure" || state == "error"))
    {
        return "failure".to_string();
    }

    if statuses.is_empty()
        || statuses
            .iter()
            .any(|(state, count)| *count > 0 && state != "success")
    {
        return "pending".to_string();
    }

    "success".to_string()
}

pub async fn get_combined_commit_status(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<Json<CombinedCommitStatusResponse>, (StatusCode, Json<ApiError>)> {
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

    let (offset, limit) = json_page(query.page, query.per_page)?;
    let statuses = db::repositories::protection::list_commit_statuses_page(
        &state.pool,
        repository.id,
        &sha,
        offset as i64,
        limit as i64,
    )
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

    let counts =
        db::repositories::protection::commit_status_counts(&state.pool, repository.id, &sha)
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ApiError::new(
                        "database_error",
                        "Status counts are unavailable",
                    )),
                )
            })?;
    let combined_state = combined_commit_status_state(&counts);
    let total_count = counts.iter().map(|(_, count)| *count as usize).sum();
    let responses = status_responses(&state.pool, statuses).await?;

    Ok(Json(CombinedCommitStatusResponse {
        sha,
        state: combined_state,
        total_count,
        statuses: responses,
    }))
}
