use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{
    date, default_ref, load_refs_from_repo, repository_default_branch, store_error, Loaded,
};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::repo_store;
use crate::webui::pages::repos as view;
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
    let actor = crate::backend::api::http::session::get_current_user(&state.pool, &headers).await?;
    let sha = crate::backend::repositories::refs::create_branch(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        actor.id,
        &owner,
        super::repository_id(&state, &owner, &repo).await?,
        &req.name,
        &req.sha,
    )
    .await
    .map_err(crate::backend::api::repos::responses::ref_error)?;
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
    let actor = crate::backend::api::http::session::get_current_user(&state.pool, &headers).await?;
    crate::backend::repositories::refs::delete_branch(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        actor.id,
        super::repository_id(&state, &owner, &repo).await?,
        &branch,
    )
    .await
    .map_err(crate::backend::api::repos::responses::ref_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Divergence counts are read for this many branches. Each count walks a branch's history back
/// to the merge base with the default branch, so a long branch list must not make the page slow.
const MAX_BRANCH_STATUSES: usize = 200;

/// A branch whose tip is older than this is stale.
const STALE_AFTER: chrono::Duration = chrono::Duration::days(90);

/// Fills in each branch's time of its tip, and for the first branches its commits ahead of and
/// behind the default branch. The default branch comes first in `branches`.
async fn fill_branch_statuses(
    ctx: &PageContext,
    loaded: &Loaded,
    branches: &mut [view::branches::RefRow],
) -> Result<(), Response> {
    let Some(base) = branches
        .iter()
        .find(|branch| branch.is_default)
        .map(|branch| branch.sha.clone())
    else {
        return Ok(());
    };
    let counted = branches.len().min(MAX_BRANCH_STATUSES);
    let heads: Vec<String> = branches[..counted]
        .iter()
        .map(|branch| branch.sha.clone())
        .collect();
    let statuses = repo_store(&loaded.repository, ctx.state.cluster.as_ref())
        .branch_statuses(&base, &heads)
        .await
        .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
    // The statuses cover the first `counted` branches, the zip stops there.
    for (branch, status) in branches.iter_mut().zip(statuses) {
        branch.updated = Some(date(status.updated));
        if !branch.is_default {
            branch.ahead_behind = Some((status.ahead, status.behind));
        }
    }
    Ok(())
}

/// Links each branch that is the head of an open pull request to it. When several are open, the
/// one with the lowest number is linked.
async fn fill_open_pulls(
    ctx: &PageContext,
    loaded: &Loaded,
    branches: &mut [view::branches::RefRow],
) -> Result<(), Response> {
    let pulls = db::pull_requests::records::list_open_pull_requests_from_branches(
        &ctx.state.pool,
        loaded.repository.id,
    )
    .await
    .map_err(|_| {
        ctx.repo_error(
            &loaded.chrome,
            StatusCode::INTERNAL_SERVER_ERROR,
            "Pull requests are unavailable.",
        )
    })?;
    for branch in branches.iter_mut() {
        branch.pull = pulls
            .iter()
            .find(|pull| pull.head_branch == branch.name)
            .map(|pull| view::pulls::PullSummary {
                number: pull.number,
                title: pull.title.clone(),
            });
    }
    Ok(())
}

pub(in crate::backend::api::repos) async fn refs_page(
    ctx: &PageContext,
    loaded: &Loaded,
    tags: bool,
) -> Result<Response, Response> {
    let default = default_ref(loaded);
    // Every branch, the default first, is where a new branch or tag can start.
    let mut branches: Vec<view::branches::RefRow> = loaded
        .refs
        .iter()
        .filter_map(|(name, sha)| {
            name.strip_prefix("refs/heads/")
                .map(|name| view::branches::RefRow {
                    name: name.into(),
                    sha: sha.clone(),
                    message: None,
                    is_default: name == default,
                    ahead_behind: None,
                    updated: None,
                    pull: None,
                })
        })
        .collect();
    branches.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.cmp(&b.name))
    });
    if !tags {
        fill_branch_statuses(ctx, loaded, &mut branches).await?;
        fill_open_pulls(ctx, loaded, &mut branches).await?;
    }
    let mut rows = if tags {
        repo_store(&loaded.repository, ctx.state.cluster.as_ref())
            .tags()
            .await
            .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?
            .into_iter()
            .map(|tag| view::branches::RefRow {
                name: tag.name,
                sha: tag.sha,
                message: tag.message,
                is_default: false,
                ahead_behind: None,
                updated: tag.date.map(date),
                pull: None,
            })
            .collect::<Vec<_>>()
    } else {
        branches.clone()
    };
    // Tags are listed by name. Branches keep the default first, then by name.
    if tags {
        rows.sort_by(|a, b| a.name.cmp(&b.name));
    }
    let query = ctx.query.get("q").cloned().unwrap_or_default();
    let tab = match ctx.query.get("tab").map(String::as_str) {
        Some(tab @ ("active" | "stale")) if !tags => tab.to_owned(),
        _ => "all".to_owned(),
    };
    let is_stale = |row: &view::branches::RefRow| {
        !row.is_default
            && row
                .updated
                .as_deref()
                .and_then(|updated| chrono::DateTime::parse_from_rfc3339(updated).ok())
                .is_some_and(|updated| updated < chrono::Utc::now() - STALE_AFTER)
    };
    let stale = branches.iter().filter(|row| is_stale(row)).count();
    if tab != "all" {
        rows.retain(|row| is_stale(row) == (tab == "stale"));
    }
    Ok(ctx.page_model(
        if tags { "Tags" } else { "Branches" },
        crate::webui::app::Page::Repositories(view::page::Page::Refs {
            repo: loaded.chrome.clone(),
            rows,
            tags,
            branches,
            query,
            tab,
            stale,
        }),
    ))
}

pub(in crate::backend::api::repos) async fn page(
    axum::extract::State(state): axum::extract::State<
        std::sync::Arc<crate::backend::app::AppState>,
    >,
    headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    axum::extract::Path(params): axum::extract::Path<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let ctx =
        match crate::backend::api::http::context::PageContext::load(state, headers, &uri).await {
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
