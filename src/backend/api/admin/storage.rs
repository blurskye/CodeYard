//! Admin storage overview page.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::user::responses::require_account;
use crate::backend::app::AppState;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::get,
    Router,
};
use codeyard_webui::{app::Page, pages::admin::storage as view};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/admin/storage", get(show))
}

/// The name of the database engine in use.
fn database_name(pool: &codeyard_db_core::DbPool) -> &'static str {
    match pool {
        codeyard_db_core::DbPool::Postgres(_) => "PostgreSQL",
        codeyard_db_core::DbPool::Sqlite(_) => "SQLite",
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
            _ => "Local directory",
        }
        .into(),
        location: ctx.state.storage.location(),
    };
    ctx.page_model_status(StatusCode::OK, "Storage", Page::Storage(model))
}
