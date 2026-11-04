use crate::access::readable_repository;
use crate::paging::json_page;
use crate::paging::PageQuery;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::shared::repos::responses::repository_unavailable;
use codeyard_api_http::shared::user::responses::PublicUserResponse as UserResponse;
use codeyard_app::app::AppState;
use std::sync::Arc;

pub(crate) async fn list_stargazers(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<PageQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Vec<UserResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    let (offset, limit) = json_page(query.page, query.per_page)?;
    let users = codeyard_db_repos::repositories::engagement::list_repo_stargazers(
        &state.pool,
        repository.id,
        limit as i64,
        offset as i64,
    )
    .await
    .map_err(|_| repository_unavailable())?;

    codeyard_api_http::shared::repos::access::check_repo_read_access(
        &state.pool,
        &repository,
        &headers,
    )
    .await?;
    Ok(Json(users.into_iter().map(UserResponse::from).collect()))
}
