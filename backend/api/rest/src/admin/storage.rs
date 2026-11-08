//! Admin storage overview page.
use crate::user::responses::require_account;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_webui::app::Page;
use codeyard_webui::pages::admin::storage as view;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().cell_route("/admin/storage", get(show))
}

/// The name of the database engine in use.
fn database_name(pool: &codeyard_database::DbPool) -> &'static str {
    match pool {
        codeyard_database::DbPool::Postgres(_) => "PostgreSQL",
        codeyard_database::DbPool::Sqlite(_) => "SQLite",
    }
}

async fn show(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = require_account(&ctx, true) {
        return response;
    }
    let model = view::Page {
        database: database_name(&ctx.state.pool).into(),
        object_storage: match ctx.state.storage.kind() {
            "s3" => "S3-compatible bucket",
            "nodes" => "Two storage-node copies",
            _ => "Local directory",
        }
        .into(),
        location: ctx.state.storage.location(),
    };
    ctx.page_model_status(StatusCode::OK, "Storage", Page::Storage(model))
}
