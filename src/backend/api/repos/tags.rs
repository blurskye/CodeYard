use crate::backend::api::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::repo_store;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
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
    let mut responses: Vec<TagResponse> = repo_store(&repository, state.cluster.as_ref())
        .tags()
        .await
        .map_err(|error| {
            super::responses::repo_store_error(error, StatusCode::INTERNAL_SERVER_ERROR)
        })?
        .into_iter()
        .map(|tag| TagResponse {
            name: tag.name,
            sha: tag.sha,
            message: tag.message,
        })
        .collect();
    responses.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Json(responses))
}

pub async fn create_tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<CreateTagRequest>,
) -> Result<Json<TagResponse>, (StatusCode, Json<ApiError>)> {
    let actor = crate::backend::api::session::get_current_user(&state.pool, &headers).await?;
    let tag = crate::backend::repositories::refs::create_tag(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        actor.id,
        &owner,
        &repo,
        &req.name,
        &req.sha,
        req.message.as_deref(),
    )
    .await
    .map_err(super::responses::ref_error)?;
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
    let actor = crate::backend::api::session::get_current_user(&state.pool, &headers).await?;
    crate::backend::repositories::refs::delete_tag(
        &state.pool,
        state.cluster.as_ref(),
        actor.id,
        &owner,
        &repo,
        &name,
    )
    .await
    .map_err(super::responses::ref_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn page(
    axum::extract::State(state): axum::extract::State<
        std::sync::Arc<crate::backend::app::AppState>,
    >,
    headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    axum::extract::Path(params): axum::extract::Path<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let ctx = match crate::backend::api::context::PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded =
        match crate::backend::api::repos::chrome::load(&ctx, &params["owner"], &params["repo"])
            .await
        {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    match crate::backend::api::repos::branches::refs_page(&ctx, &loaded, true).await {
        Ok(response) | Err(response) => response,
    }
}
