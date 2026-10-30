//! The "Go to file" page: match the complete tree before retaining bounded paths.
//! Complete unfiltered listings can also be narrowed without a round trip.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::chrome::{default_ref, load, resolve, store_error, Loaded};
use crate::backend::app::AppState;
use crate::backend::repositories::store::repo_store;
use crate::webui::pages::repos as view;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use std::sync::Arc;

async fn find_page(ctx: &PageContext, loaded: &Loaded, tail: &str) -> Result<Response, Response> {
    let resolved = if tail.is_empty() {
        view::ref_path::ResolvedRefPath {
            reference: default_ref(loaded),
            path: String::new(),
        }
    } else {
        resolve(ctx, loaded, tail, false)?
    };
    if !resolved.path.is_empty() {
        // The page lists one revision, so a path after the revision is not a page here.
        return Err(ctx.error(
            StatusCode::NOT_FOUND,
            "Go to file searches one branch, tag or commit.",
        ));
    }
    if loaded.refs.is_empty() {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Repository is empty."));
    }
    let store = repo_store(&loaded.repository, ctx.state.cluster.as_ref());
    let reference = if !super::super::chrome::is_branch(loaded, &resolved.reference)
        && !resolved.reference.starts_with("refs/")
    {
        store
            .commit(&resolved.reference)
            .await
            .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?
            .sha
    } else {
        resolved.reference
    };
    let query: String = ctx.query.get("q").map(|query| query.chars().take(256).collect()).unwrap_or_default();
    let files = store
        .file_paths_matching(&reference, &query, view::find::MAX_LISTED_FILES)
        .await
        .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
    super::super::access::check_repo_read_access(&ctx.state.pool, &loaded.repository, &ctx.headers)
        .await.map_err(|_| ctx.error(StatusCode::NOT_FOUND, "Repository not found."))?;
    let page = view::find::FindPage {
        reference,
        paths: files.paths,
        truncated: files.truncated,
        query,
    };
    Ok(ctx.page_model(
        &format!(
            "Go to file · {}/{}",
            loaded.chrome.owner, loaded.chrome.name
        ),
        crate::webui::app::Page::Repositories(view::page::Page::Find {
            repo: loaded.chrome.clone(),
            page,
        }),
    ))
}

pub(in crate::backend::api::repos) async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    super::GitPathParams(params): super::GitPathParams,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match load(&ctx, &params["owner"], &params["repo"]).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let tail = params.get("ref_path").map(String::as_str).unwrap_or("");
    match find_page(&ctx, &loaded, tail).await {
        Ok(response) | Err(response) => response,
    }
}
