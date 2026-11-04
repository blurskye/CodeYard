//! The actor's organization collection, using the same authorized settings projection.

use crate::backend::api::http::context::PageContext;
use crate::backend::api::organizations::organization::Fields;
use crate::backend::app::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
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
    crate::backend::api::organizations::organization::respond(
        &ctx,
        None,
        StatusCode::OK,
        None,
        Fields::new(),
    )
    .await
}
