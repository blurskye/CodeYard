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
    http::{HeaderMap, Uri},
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
    let reader = crate::backend::db::dependency_graph::Reader::Viewer(ctx.user.as_ref().map(|user| ctx.database_actor(user.id)));
    let (sha, packages) =
        match graph::inventory(&ctx.state, &reader, &loaded.repository, &loaded.default_branch).await {
            Ok(inventory) => inventory,
            Err(error) => return chrome::store_error(&ctx, Some(&loaded.chrome), error),
        };
    if download {
        let mut document = graph::sbom(&owner, &name, &sha, &packages);
        return Json(document["sbom"].take()).into_response();
    }
    let packages = packages
        .into_iter()
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
            page: GraphPage { sha, packages },
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
