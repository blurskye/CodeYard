use crate::backend::api::context::PageContext;
use crate::backend::api::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{
    default_ref, load_refs_from_repo, repository_default_branch, store_error, Loaded,
};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::repo_store;
use crate::webui::pages::repositories as view;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Serialize)]
pub struct BranchResponse {
    pub name: String,
    pub is_default: bool,
}

#[derive(Deserialize)]
pub struct CreateBranchRequest {
    pub name: String,
    pub sha: String,
}

#[derive(Serialize)]
pub struct CreateBranchResponse {
    pub name: String,
    pub sha: String,
}

pub async fn list_branches(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<BranchResponse>>, (StatusCode, Json<ApiError>)> {
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
    let refs = load_refs_from_repo(&state, &repository, &format!("{}/{}", owner, repo)).await?;
    let mut branches: Vec<String> = refs
        .keys()
        .filter_map(|name| {
            name.strip_prefix("refs/heads/")
                .map(|branch| branch.to_string())
        })
        .collect();
    branches.sort();

    let default_branch = crate::backend::repositories::reads::default_branch(
        &refs,
        &repository_default_branch(&state, &repository).await?,
        &repository.default_branch,
    );
    Ok(Json(
        branches
            .into_iter()
            .map(|name| BranchResponse {
                is_default: name == default_branch,
                name,
            })
            .collect(),
    ))
}

pub async fn create_branch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<CreateBranchRequest>,
) -> Result<Json<CreateBranchResponse>, (StatusCode, Json<ApiError>)> {
    let actor = crate::backend::api::session::get_current_user(&state.pool, &headers).await?;
    let sha = crate::backend::repositories::refs::create_branch(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        actor.id,
        &owner,
        &repo,
        &req.name,
        &req.sha,
    )
    .await
    .map_err(super::responses::ref_error)?;
    Ok(Json(CreateBranchResponse {
        name: req.name,
        sha,
    }))
}

pub async fn delete_branch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, branch)): Path<(String, String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let actor = crate::backend::api::session::get_current_user(&state.pool, &headers).await?;
    crate::backend::repositories::refs::delete_branch(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        actor.id,
        &owner,
        &repo,
        &branch,
    )
    .await
    .map_err(super::responses::ref_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn refs_page(
    ctx: &PageContext,
    loaded: &Loaded,
    tags: bool,
) -> Result<Response, Response> {
    let default = default_ref(loaded);
    // Every branch, the default first, is where a new branch or tag can start.
    let mut branches: Vec<view::RefRow> = loaded
        .refs
        .iter()
        .filter_map(|(name, sha)| {
            name.strip_prefix("refs/heads/").map(|name| view::RefRow {
                name: name.into(),
                sha: sha.clone(),
                message: None,
                is_default: name == default,
            })
        })
        .collect();
    branches.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut rows = if tags {
        repo_store(&loaded.repository, ctx.state.cluster.as_ref())
            .tags()
            .await
            .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?
            .into_iter()
            .map(|tag| view::RefRow {
                name: tag.name,
                sha: tag.sha,
                message: tag.message,
                is_default: false,
            })
            .collect::<Vec<_>>()
    } else {
        branches.clone()
    };
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ctx.page_model(
        if tags { "Tags" } else { "Branches" },
        crate::webui::app::Page::Repositories(view::Page::Refs {
            repo: loaded.chrome.clone(),
            rows,
            tags,
            branches,
        }),
    ))
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
    match refs_page(&ctx, &loaded, false).await {
        Ok(response) | Err(response) => response,
    }
}
