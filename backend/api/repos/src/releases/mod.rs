//! Releases: the list and pages of a repository's releases, their native forms, asset uploads and
//! downloads, and the JSON API. The domain module owns the rules; these adapters map them.
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use codeyard_app::app::AppState;
use std::sync::Arc;
mod attestation;
mod json;
mod pages;
mod reactions;

/// The largest asset body: the file limit plus room for the form fields.
const UPLOAD_BODY_LIMIT: usize = codeyard_model::release_integrity::MAX_ASSET_BYTES + 1024 * 1024;

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/repos/:owner/:repo/releases",
            get(crate::releases::json::list).post(crate::releases::json::create),
        )
        .route(
            "/api/repos/:owner/:repo/releases/tags/*tag",
            get(crate::releases::json::get_by_tag),
        )
        .route(
            "/api/repos/:owner/:repo/releases/:id",
            get(crate::releases::json::get_one)
                .patch(crate::releases::json::update)
                .delete(crate::releases::json::delete),
        )
}

/// Release pages, forms and downloads. The caller applies the form body limit.
pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/releases", get(crate::releases::pages::list))
        .route(
            "/:owner/:repo/releases/new",
            get(crate::releases::pages::new_page).post(crate::releases::pages::create),
        )
        .route(
            "/:owner/:repo/releases/tag/*tag",
            get(crate::releases::pages::detail),
        )
        .route(
            "/:owner/:repo/releases/edit/:id",
            get(crate::releases::pages::edit_page).post(crate::releases::pages::update),
        )
        .route(
            "/:owner/:repo/releases/:id/delete",
            post(crate::releases::pages::delete),
        )
        .route(
            "/:owner/:repo/releases/:id/react",
            post(crate::releases::reactions::react),
        )
        .route(
            "/:owner/:repo/releases/:id/attestation",
            get(crate::releases::attestation::download),
        )
        .route(
            "/:owner/:repo/releases/:id/assets/:asset/edit",
            post(crate::releases::pages::update_asset),
        )
        .route(
            "/:owner/:repo/releases/:id/assets/:asset/delete",
            post(crate::releases::pages::delete_asset),
        )
        .route(
            "/:owner/:repo/releases/download/*asset",
            get(crate::releases::pages::download),
        )
}

/// Asset uploads carry whole files, so they have their own, larger body limit.
pub fn upload_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(ASSET_ROUTE, post(crate::releases::pages::upload_asset))
        .layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT))
}

use codeyard_api_http::shared::repos::releases::ASSET_ROUTE;
