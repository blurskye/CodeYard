use super::chrome;
use crate::backend::{
    api::http::context::PageContext, app::AppState, repositories::dependency_graph as graph,
};
use crate::webui::{
    app::Page,
    pages::repos::{
        dependency_graph::{Dependency, Page as GraphPage},
        page::Page as RepoPage,
    },
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::sync::Arc;

pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/dependency-graph", get(page))
        .route(
            "/:owner/:repo/dependency-graph/sbom",
            get(download).layer(axum::middleware::from_fn(
                crate::backend::api::http::middleware::private_api_response,
            )),
        )
}

async fn respond(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    owner: String,
    name: String,
    download: bool,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match chrome::load_database(&ctx, &owner, &name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let reference = format!("refs/heads/{}", loaded.default_branch);
    let (sha, packages) =
        match graph::inventory(&ctx.state, &loaded.repository, &reference).await {
            Ok(inventory) => inventory,
            Err(error) if error.kind == crate::backend::repositories::store::ErrorKind::Invalid => {
                return ctx.error(StatusCode::UNPROCESSABLE_ENTITY, &error.message);
            }
            Err(error) => return chrome::store_error(&ctx, Some(&loaded.chrome), error),
        };
    if let Err((status, _)) = super::access::check_repo_read_access(&ctx.state.pool, &loaded.repository, &ctx.headers).await {
        return ctx.error(status, "Repository not found.");
    }
    if download {
        let mut document = graph::sbom(&owner, &name, &sha, &packages);
        return Json(document["sbom"].take()).into_response();
    }
    let number = match super::paging::number(&ctx, "page", 1, 1..=super::paging::MAX_PAGE) {
        Ok(number) => number,
        Err(response) => return response,
    };
    let per_page = super::paging::DEFAULT_PER_PAGE;
    let offset = (number - 1) * per_page;
    let link = |page| format!("{}/dependency-graph?page={page}", loaded.chrome.base());
    let previous = (number > 1).then(|| link(number - 1));
    let next = (offset + per_page < packages.len()).then(|| link(number + 1));
    let packages = packages
        .into_iter()
        .skip(offset)
        .take(per_page)
        .map(|package| Dependency {
            name: package.name,
            version: package.version,
            manifest: package.manifest,
            ecosystem: package.ecosystem,
            scope: package.scope,
        })
        .collect();
    ctx.page_model(
        "Dependency graph",
        Page::Repositories(RepoPage::DependencyGraph {
            repo: loaded.chrome,
            page: GraphPage { sha, packages, previous, next },
        }),
    )
}

async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    respond(state, headers, uri, owner, name, false).await
}
async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    respond(state, headers, uri, owner, name, true).await
}
