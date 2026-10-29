use crate::backend::app::AppState;
use axum::{routing::get, Router};
use std::sync::Arc;
pub mod collection;
mod comments;
pub mod commit;
pub mod status;
pub mod statuses;

pub fn router() -> Router<Arc<AppState>> {
    let json = json_router()
        .route_layer(axum::middleware::from_fn(
            crate::backend::api::http::middleware::require_node_access,
        ))
        .route_layer(axum::middleware::from_fn(
            crate::backend::api::http::csrf::cookie_api_submission,
        ))
        .layer(axum::middleware::from_fn(
            crate::backend::api::http::middleware::private_api_response,
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
        .route(
            "/:owner/:repo/commit/:sha/comments",
            axum::routing::post(comments::submit),
        )
        .route(
            "/:owner/:repo/commit/:sha/comments/:id/react",
            axum::routing::post(comments::react),
        )
        .route(
            "/:owner/:repo/commit/:sha/comments/:id/edit",
            axum::routing::post(comments::submit),
        )
        .route(
            "/:owner/:repo/commit/:sha/comments/:id/delete",
            axum::routing::post(comments::submit),
        )
        // A maximally percent-encoded 65,536-scalar comment still fits.
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
}
