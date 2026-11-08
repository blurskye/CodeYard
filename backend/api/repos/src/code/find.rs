//! The "Go to file" page: match the complete tree before retaining bounded paths.
//! Complete unfiltered listings can also be narrowed without a round trip.
use crate::chrome::default_ref;
use crate::chrome::load;
use crate::chrome::resolve;
use axum::extract::OriginalUri;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::repos::chrome::store_error;
use codeyard_api_http::shared::repos::chrome::Loaded;
use codeyard_app::app::AppState;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_webui::pages::repos as view;
use std::sync::Arc;

async fn find_page(ctx: &PageContext, loaded: &Loaded, tail: &str) -> Result<Response, Response> {
    let resolved = if tail.is_empty() {
        codeyard_types::ref_path::ResolvedRefPath {
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
    let reference = if !crate::chrome::is_branch(loaded, &resolved.reference)
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
    let query: String = ctx
        .query
        .get("q")
        .map(|query| query.chars().take(256).collect())
        .unwrap_or_default();
    let files = store
        .file_paths_matching(&reference, &query, view::find::MAX_LISTED_FILES)
        .await
        .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
    codeyard_api_http::shared::repos::access::check_repo_read_access(
        &ctx.state.pool,
        &loaded.repository,
        &ctx.headers,
    )
    .await
    .map_err(|_| ctx.error(StatusCode::NOT_FOUND, "Repository not found."))?;
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
        codeyard_webui::app::Page::Repositories(view::page::Page::Find {
            repo: loaded.chrome.clone(),
            page,
        }),
    ))
}

pub(crate) async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    crate::code::GitPathParams(params): crate::code::GitPathParams,
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
