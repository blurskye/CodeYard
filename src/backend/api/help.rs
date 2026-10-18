//! The help page. It needs no account, and the header shows the viewer's account when there is one.
use crate::backend::{api::http::context::PageContext, app::AppState};
use crate::webui::app::Page as AppPage;
use axum::{
    extract::State,
    http::{HeaderMap, Uri},
    response::Response,
    routing::get,
    Router,
};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/help", get(help_page))
}

async fn help_page(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    ctx.page_model("Help", AppPage::Help)
}
