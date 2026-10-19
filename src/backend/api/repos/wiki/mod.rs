//! The repository wiki: pages, their revisions and forms, and a read-only JSON API. The domain rules
//! are in `repositories::wiki`; the database checks authority with each write.
mod json;
mod pages;

use crate::backend::app::AppState;
use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use std::sync::Arc;

/// Page bodies are at most 1 MiB; the form fields around them need a little room.
const FORM_BODY_LIMIT: usize = crate::backend::repositories::wiki::BODY_LIMIT + 64 * 1024;

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/repos/:owner/:repo/wiki/pages", get(json::list))
        .route(
            "/api/repos/:owner/:repo/wiki/pages/:slug",
            get(json::get_one),
        )
}

/// Wiki pages and forms. The caller applies the form body limit.
pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/wiki", get(pages::home))
        .route("/:owner/:repo/wiki/_pages", get(pages::list))
        .route(
            "/:owner/:repo/wiki/_new",
            get(pages::new_page).post(pages::create),
        )
        .route("/:owner/:repo/wiki/_settings", post(pages::set_enabled))
        .route("/:owner/:repo/wiki/:slug", get(pages::show))
        .route(
            "/:owner/:repo/wiki/:slug/edit",
            get(pages::edit_page).post(pages::update),
        )
        .route("/:owner/:repo/wiki/:slug/delete", post(pages::delete_page))
        .route("/:owner/:repo/wiki/:slug/history", get(pages::history))
        .route(
            "/:owner/:repo/wiki/:slug/revisions/:number",
            get(pages::revision),
        )
        .route(
            "/:owner/:repo/wiki/:slug/compare/:old/:new",
            get(pages::compare),
        )
        .layer(DefaultBodyLimit::max(FORM_BODY_LIMIT))
}
