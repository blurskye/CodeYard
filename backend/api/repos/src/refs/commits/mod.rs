use axum::routing::get;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_app::app::AppState;
use std::sync::Arc;

pub mod collection;
mod comments;
pub mod commit;
pub mod status;
pub mod statuses;

pub fn router() -> Router<Arc<AppState>> {
    let json = json_router()
        .route_layer(axum::middleware::from_fn(
            codeyard_api_http::middleware::require_node_access,
        ))
        .route_layer(axum::middleware::from_fn(
            codeyard_api_http::csrf::cookie_api_submission,
        ))
        .layer(axum::middleware::from_fn(
            codeyard_api_http::middleware::private_api_response,
        ));
    json.merge(page_router().layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024)))
}

fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route(
            "/api/repos/:owner/:repo/commits",
            get(crate::refs::commits::collection::list_commits),
        )
        .cell_route(
            "/api/repos/:owner/:repo/commits/:sha",
            get(crate::refs::commits::commit::get_commit),
        )
        .cell_route(
            "/api/repos/:owner/:repo/commits/:sha/status",
            get(crate::refs::commits::status::get_combined_commit_status),
        )
        .cell_route(
            "/api/repos/:owner/:repo/commits/:sha/statuses",
            get(crate::refs::commits::statuses::list_commit_statuses)
                .post(crate::refs::commits::statuses::create_commit_status),
        )
}

fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route(
            "/:owner/:repo/commits",
            get(crate::refs::commits::collection::page),
        )
        .cell_route(
            "/:owner/:repo/commits/*git_ref",
            get(crate::refs::commits::collection::ref_redirect),
        )
        .cell_route(
            "/:owner/:repo/commit/:sha",
            get(crate::refs::commits::commit::page),
        )
        .cell_route(
            "/:owner/:repo/commit/:sha/comments",
            axum::routing::post(crate::refs::commits::comments::submit),
        )
        .cell_route(
            "/:owner/:repo/commit/:sha/comments/:id/react",
            axum::routing::post(crate::refs::commits::comments::react),
        )
        .cell_route(
            "/:owner/:repo/commit/:sha/comments/:id/edit",
            axum::routing::post(crate::refs::commits::comments::submit),
        )
        .cell_route(
            "/:owner/:repo/commit/:sha/comments/:id/delete",
            axum::routing::post(crate::refs::commits::comments::submit),
        )
        // A maximally percent-encoded 65,536-scalar comment still fits.
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
}
