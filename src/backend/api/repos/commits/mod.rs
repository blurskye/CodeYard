use crate::backend::app::AppState;
use axum::{routing::get, Router};
use std::sync::Arc;
pub mod collection;
pub mod commit;
pub mod status;
pub mod statuses;

pub fn router() -> Router<Arc<AppState>> {
    let json = json_router()
        .route_layer(axum::middleware::from_fn(
            crate::backend::api::middleware::require_node_access,
        ))
        .route_layer(axum::middleware::from_fn(
            crate::backend::api::csrf::cookie_api_submission,
        ))
        .layer(axum::middleware::from_fn(
            crate::backend::api::middleware::private_api_response,
        ));
    json.merge(page_router().layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024)))
}

fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/repos/:owner/:repo/commits",
            get(collection::list_commits),
        )
        .route(
            "/api/repos/:owner/:repo/commits/:sha",
            get(commit::get_commit),
        )
        .route(
            "/api/repos/:owner/:repo/commits/:sha/status",
            get(status::get_combined_commit_status),
        )
        .route(
            "/api/repos/:owner/:repo/commits/:sha/statuses",
            get(statuses::list_commit_statuses).post(statuses::create_commit_status),
        )
}

fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/commits", get(collection::page))
        .route(
            "/:owner/:repo/commits/*git_ref",
            get(collection::ref_redirect),
        )
        .route("/:owner/:repo/commit/:sha", get(commit::page))
}
