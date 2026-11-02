use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::readable_repository;
use crate::backend::api::repos::paging::{json_page, PageQuery};
use crate::backend::api::repos::responses::repository_unavailable;
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
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    let (offset, limit) = json_page(query.page, query.per_page)?;
    let users = db::repositories::engagement::list_repo_stargazers(
        &state.pool,
        repository.id,
        limit as i64,
        offset as i64,
    )
    .await
    .map_err(|_| repository_unavailable())?;

    Ok(Json(users.into_iter().map(UserResponse::from).collect()))
}
