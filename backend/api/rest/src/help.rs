//! The help page. It needs no account, and the header shows the viewer's account when there is one.
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_webui::app::Page as AppPage;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().cell_route("/help", get(help_page))
}

async fn help_page(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    ctx.page_model("Help", AppPage::Help)
}
