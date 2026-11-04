//! Refs: branches, tags, commit history and comparisons between refs.
use axum::routing::delete;
use axum::routing::get;
use axum::Router;
use codeyard_app::app::AppState;
use std::sync::Arc;

pub mod branches;
pub mod commits;
pub mod compare;
mod merge;
pub mod references;
pub mod tags;

async fn repository_id(
    state: &AppState,
    owner: &str,
    name: &str,
) -> codeyard_api_http::shared::repos::chrome::ApiResult<i64> {
    crate::access::find_repository(&state.pool, owner, name)
        .await
        .map(|repo| repo.id)
}

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .merge(crate::refs::merge::json_routes())
        .route(
            "/api/repos/:owner/:repo/branches",
            get(crate::refs::branches::list_branches).post(crate::refs::branches::create_branch),
        )
        .route(
            "/api/repos/:owner/:repo/branches/:branch",
            delete(crate::refs::branches::delete_branch),
        )
        .route(
            "/api/repos/:owner/:repo/tags",
            get(crate::refs::tags::list_tags).post(crate::refs::tags::create_tag),
        )
        .route(
            "/api/repos/:owner/:repo/tags/:name",
            delete(crate::refs::tags::delete_tag),
        )
}

pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .merge(crate::refs::merge::pages())
        .route("/:owner/:repo/branches", get(crate::refs::branches::page))
        .route(
            "/:owner/:repo/compare",
            get(crate::refs::compare::native_compare_page),
        )
        .route(
            "/:owner/:repo/compare/*spec",
            get(crate::refs::compare::native_compare_page),
        )
        .route("/:owner/:repo/tags", get(crate::refs::tags::page))
}
