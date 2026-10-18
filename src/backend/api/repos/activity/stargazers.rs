use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{json_page, PageQuery};
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use std::sync::Arc;
pub async fn list_stargazers(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<PageQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Vec<UserResponse>>, (StatusCode, Json<ApiError>)> {
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
    let (offset, limit) = json_page(query.page, query.per_page)?;
    let users = db::repositories::engagement::list_repo_stargazers(
        &state.pool,
        repository.id,
        limit as i64,
        offset as i64,
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

    Ok(Json(users.into_iter().map(UserResponse::from).collect()))
}
