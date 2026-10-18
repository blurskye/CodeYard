//! The "Go to file" page: every file path at one branch, tag or commit. The `q` query is
//! applied by the page view, so the same list filters without a round trip.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::chrome::{default_ref, load, resolve, store_error, Loaded};
use crate::backend::app::AppState;
use crate::backend::repositories::store::repo_store;
use crate::webui::pages::repos as view;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use std::collections::HashMap;
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
    let files = store
        .file_paths(&resolved.reference, view::find::MAX_LISTED_FILES)
        .await
        .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
    let page = view::find::FindPage {
        reference: resolved.reference,
        paths: files.paths,
        truncated: files.truncated,
        query: ctx.query.get("q").cloned().unwrap_or_default(),
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
    Path(params): Path<HashMap<String, String>>,
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
