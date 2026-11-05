use axum::middleware::from_fn;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_app::app::AppState;
use std::sync::Arc;
mod collection;
mod confirm;
mod content;
pub(crate) mod download;
mod file;
mod public;
mod stats;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // Axum's previous nested collection accepted both spellings.
        .cell_route(
            "/api/files",
            post(crate::files::collection::create_upload_url),
        )
        .cell_route(
            "/api/files/",
            post(crate::files::collection::create_upload_url),
        )
        .cell_route(
            "/api/files/confirm",
            post(crate::files::confirm::confirm_upload),
        )
        .cell_route(
            "/api/files/stats",
            get(crate::files::stats::get_my_file_stats),
        )
        .cell_route(
            "/api/files/:file_id/dl",
            get(crate::files::download::get_download_url),
        )
        .cell_route(
            "/api/files/:file_id/download",
            get(crate::files::public::public_file_redirect),
        )
        .cell_route(
            "/api/files/public/:file_id",
            get(crate::files::public::get_public_download_url),
        )
        .cell_route(
            CONTENT_ROUTE,
            get(crate::files::public::public_file_redirect)
                .put(crate::files::content::put_upload_content),
        )
        .cell_route(
            "/api/files/:file_id",
            delete(crate::files::file::delete_file),
        )
        .route_layer(from_fn(codeyard_api_http::csrf::cookie_api_submission))
        .route_layer(from_fn(codeyard_api_http::middleware::require_node_access))
        .layer(from_fn(codeyard_api_http::middleware::private_api_response))
}

use codeyard_api_http::shared::files::CONTENT_ROUTE;
