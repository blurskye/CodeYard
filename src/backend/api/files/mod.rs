mod collection;
mod confirm;
mod content;
mod download;
mod file;
mod public;
mod stats;

use crate::backend::{api::error::ApiError, app::AppState};
use axum::{
    http::StatusCode,
    middleware::from_fn,
    routing::{delete, get, post},
    Json, Router,
};
use std::sync::Arc;

type Failure = (StatusCode, Json<ApiError>);

fn failure(status: StatusCode, code: &str, message: &str) -> Failure {
    (status, Json(ApiError::new(code, message)))
}

/// The streamed file upload route. The request-body gate skips its PUT, which reads the
/// body as it arrives.
pub(crate) const CONTENT_ROUTE: &str = "/api/files/:file_id/content";

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // Axum's previous nested collection accepted both spellings.
        .route("/api/files", post(collection::create_upload_url))
        .route("/api/files/", post(collection::create_upload_url))
        .route("/api/files/confirm", post(confirm::confirm_upload))
        .route("/api/files/stats", get(stats::get_my_file_stats))
        .route("/api/files/:file_id/dl", get(download::get_download_url))
        .route(
            "/api/files/:file_id/download",
            get(public::public_file_redirect),
        )
        .route(
            "/api/files/public/:file_id",
            get(public::get_public_download_url),
        )
        .route(
            CONTENT_ROUTE,
            get(public::public_file_redirect).put(content::put_upload_content),
        )
        .route("/api/files/:file_id", delete(file::delete_file))
        .route_layer(from_fn(
            crate::backend::api::middleware::require_node_access,
        ))
        .layer(from_fn(
            crate::backend::api::middleware::private_api_response,
        ))
}
