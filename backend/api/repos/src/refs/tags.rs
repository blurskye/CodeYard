use crate::access::readable_repository;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_app::app::AppState;
use codeyard_repos_core::repositories::store::repo_store;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
pub struct TagResponse {
    pub name: String,
    pub sha: String,
    pub message: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateTagRequest {
    pub name: String,
    pub sha: String,
    pub message: Option<String>,
}

pub async fn list_tags(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<TagResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    let mut responses: Vec<TagResponse> = repo_store(&repository, state.cluster.as_ref())
        .tags()
        .await
        .map_err(|error| {
            crate::responses::repo_store_error(error, StatusCode::INTERNAL_SERVER_ERROR)
        })?
        .into_iter()
        .map(|tag| TagResponse {
            name: tag.name,
            sha: tag.sha,
            message: tag.message,
        })
        .collect();
    responses.sort_by(|a, b| a.name.cmp(&b.name));

    codeyard_api_http::shared::repos::access::check_repo_read_access(
        &state.pool,
        &repository,
        &headers,
    )
    .await?;
    Ok(Json(responses))
}

pub async fn create_tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<CreateTagRequest>,
) -> Result<Json<TagResponse>, (StatusCode, Json<ApiError>)> {
    let actor = codeyard_api_http::session::get_current_user(&state.pool, &headers).await?;
    let tag = codeyard_ci_engine::refs::create_tag(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        codeyard_db_core::queries::credentials::Actor::from((
            actor.id,
            codeyard_api_http::session::extract_session_id(&headers).unwrap_or_default(),
        )),
        &owner,
        crate::refs::repository_id(&state, &owner, &repo).await?,
        &req.name,
        &req.sha,
        req.message.as_deref(),
    )
    .await
    .map_err(crate::responses::ref_error)?;
    Ok(Json(TagResponse {
        name: req.name,
        sha: tag.sha,
        message: tag.message,
    }))
}

pub async fn delete_tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, name)): Path<(String, String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let actor = codeyard_api_http::session::get_current_user(&state.pool, &headers).await?;
    codeyard_ci_engine::refs::delete_tag(
        &state.pool,
        state.cluster.as_ref(),
        codeyard_db_core::queries::credentials::Actor::from((
            actor.id,
            codeyard_api_http::session::extract_session_id(&headers).unwrap_or_default(),
        )),
        crate::refs::repository_id(&state, &owner, &repo).await?,
        &name,
    )
    .await
    .map_err(crate::responses::ref_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn page(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<codeyard_app::app::AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    axum::extract::Path(params): axum::extract::Path<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let ctx = match codeyard_api_http::context::PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match crate::chrome::load(&ctx, &params["owner"], &params["repo"]).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    match crate::refs::branches::refs_page(&ctx, &loaded, true).await {
        Ok(response) | Err(response) => response,
    }
}
