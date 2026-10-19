//! Releases: the list and pages of a repository's releases, their native forms, asset uploads and
//! downloads, and the JSON API. The domain module owns the rules; these adapters map them.
mod json;
mod pages;

use crate::backend::app::AppState;
use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use std::sync::Arc;

/// The asset upload route, shared with the form-body middleware that exempts it.
pub const ASSET_ROUTE: &str = "/:owner/:repo/releases/:id/assets";
/// The largest asset body: the file limit plus room for the form fields.
const UPLOAD_BODY_LIMIT: usize =
    crate::backend::repositories::releases::MAX_ASSET_BYTES + 1024 * 1024;

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/repos/:owner/:repo/releases",
            get(json::list).post(json::create),
        )
        .route(
            "/api/repos/:owner/:repo/releases/tags/*tag",
            get(json::get_by_tag),
        )
        .route(
            "/api/repos/:owner/:repo/releases/:id",
            get(json::get_one).patch(json::update).delete(json::delete),
        )
}

/// Release pages, forms and downloads. The caller applies the form body limit.
pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/releases", get(pages::list))
        .route(
            "/:owner/:repo/releases/new",
            get(pages::new_page).post(pages::create),
        )
        .route("/:owner/:repo/releases/tag/*tag", get(pages::detail))
        .route(
            "/:owner/:repo/releases/edit/:id",
            get(pages::edit_page).post(pages::update),
        )
        .route("/:owner/:repo/releases/:id/delete", post(pages::delete))
        .route(
            "/:owner/:repo/releases/:id/assets/:asset/delete",
            post(pages::delete_asset),
        )
        .route(
            "/:owner/:repo/releases/download/*asset",
            get(pages::download),
        )
}

/// Asset uploads carry whole files, so they have their own, larger body limit.
pub fn upload_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(ASSET_ROUTE, post(pages::upload_asset))
        .layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT))
}
