//! The actor's organization collection, using the same authorized settings projection.

use crate::organizations::organization::Fields;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use std::sync::Arc;

pub(super) async fn show(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    crate::organizations::organization::respond(&ctx, None, StatusCode::OK, None, Fields::new())
        .await
}
