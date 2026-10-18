//! Refs: branches, tags, commit history and comparisons between refs.
use crate::backend::app::AppState;
use axum::{
    routing::{delete, get},
    Router,
};
use std::sync::Arc;

pub mod branches;
pub mod commits;
pub mod compare;
pub mod references;
pub mod tags;

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/repos/:owner/:repo/branches",
            get(branches::list_branches).post(branches::create_branch),
        )
        .route(
            "/api/repos/:owner/:repo/branches/:branch",
            delete(branches::delete_branch),
        )
        .route(
            "/api/repos/:owner/:repo/tags",
            get(tags::list_tags).post(tags::create_tag),
        )
        .route(
            "/api/repos/:owner/:repo/tags/:name",
            delete(tags::delete_tag),
        )
}

pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/branches", get(branches::page))
        .route("/:owner/:repo/compare", get(compare::native_compare_page))
        .route(
            "/:owner/:repo/compare/*spec",
            get(compare::native_compare_page),
        )
        .route("/:owner/:repo/tags", get(tags::page))
}
