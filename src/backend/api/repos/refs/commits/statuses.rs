use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::{
    check_repo_write_access, find_repository, readable_repository,
};
use crate::backend::api::repos::chrome::ApiResult;
use crate::backend::api::repos::paging::{json_page, PageQuery};
use crate::backend::api::repos::responses::{
    map_commit_status_response, repo_store_error, repository_unavailable,
};
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::repo_store;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Serialize)]
pub struct CommitStatusResponse {
    pub id: i64,
    pub sha: String,
    pub context: String,
    pub state: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub creator_id: Option<i64>,
    pub creator: Option<UserResponse>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Deserialize)]
pub struct CreateCommitStatusRequest {
    pub state: String,
    pub context: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
}

fn is_valid_commit_status_state(state: &str) -> bool {
    matches!(state, "pending" | "success" | "failure" | "error")
}

pub async fn list_commit_statuses(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<Json<Vec<CommitStatusResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    if !crate::backend::git::is_full_commit_id(&sha) {
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
    .map_err(|_| repository_unavailable())?;

    Ok(Json(status_responses(&state.pool, statuses).await?))
}

pub(super) async fn status_responses(
    pool: &db::DbPool,
    statuses: Vec<db::models::CommitStatus>,
) -> ApiResult<Vec<CommitStatusResponse>> {
    let mut ids: Vec<_> = statuses
        .iter()
        .filter_map(|status| status.creator_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let users = db::users::accounts::get_users_by_ids(pool, &ids)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new(
                    "database_error",
                    "Status creators are unavailable",
                )),
            )
        })?;
    let users: std::collections::HashMap<_, _> = users
        .into_iter()
        .map(|user| (user.id, UserResponse::from(user)))
        .collect();
    Ok(statuses
        .into_iter()
        .map(|status| {
            let creator = status.creator_id.and_then(|id| users.get(&id)).cloned();
            map_commit_status_response(status, creator)
        })
        .collect())
}

pub async fn create_commit_status(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(req): Json<CreateCommitStatusRequest>,
) -> Result<(StatusCode, Json<CommitStatusResponse>), (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;

    check_repo_write_access(&state.pool, &repository, &user).await?;
    if !crate::backend::git::is_full_commit_id(&sha) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "sha must be a full 40-character hexadecimal commit id",
            )),
        ));
    }
    let sha = sha.to_ascii_lowercase();

    let state_value = req.state.trim().to_lowercase();
    if !is_valid_commit_status_state(&state_value) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "state must be one of: pending, success, failure, error",
            )),
        ));
    }

    let context = req.context.trim();
    if db::repositories::protection::server_owned_status_context(context) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiError::new(
                "reserved_context",
                "This status context is managed by the Actions executor",
            )),
        ));
    }
    if context.is_empty() || context.len() > 255 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "context must be 1-255 characters",
            )),
        ));
    }

    let description = req
        .description
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let target_url = req
        .target_url
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    // A status belongs to a commit of this repository.
    repo_store(&repository, state.cluster.as_ref())
        .commit(&sha)
        .await
        .map_err(|error| repo_store_error(error, StatusCode::SERVICE_UNAVAILABLE))?;

    let status = db::repositories::protection::upsert_commit_status(
        &state.pool,
        db::repositories::protection::CommitStatusUpsert {
            repo_id: repository.id,
            sha: &sha,
            context,
            state: &state_value,
            description,
            target_url,
            creator_id: Some(user.id),
        },
    )
    .await
    .map_err(crate::backend::api::http::error::protection_write_error)?;

    Ok((
        StatusCode::CREATED,
        Json(map_commit_status_response(
            status,
            Some(UserResponse::from(user)),
        )),
    ))
}
