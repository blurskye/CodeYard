use crate::backend::api::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::commits::statuses::{is_valid_sha1_hex, CommitStatusResponse};
use crate::backend::api::repos::responses::map_commit_status_response;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use axum::extract::{Path, State};
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

fn combined_commit_status_state(statuses: &[db::models::CommitStatus]) -> String {
    if statuses
        .iter()
        .any(|status| status.state == "failure" || status.state == "error")
    {
        return "failure".to_string();
    }

    if statuses.is_empty() || statuses.iter().any(|status| status.state != "success") {
        return "pending".to_string();
    }

    "success".to_string()
}

pub async fn get_combined_commit_status(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
    headers: HeaderMap,
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

    let combined_state = combined_commit_status_state(&statuses);
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

    Ok(Json(CombinedCommitStatusResponse {
        sha,
        state: combined_state,
        total_count: responses.len(),
        statuses: responses,
    }))
}
