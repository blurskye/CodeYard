//! Admin storage overview page.
use crate::backend::api::http::context::PageContext;
use crate::backend::{app::AppState, db};
use crate::webui::{app::Page, pages::admin::storage as view};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::get,
    Router,
};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/admin/storage", get(show))
}

/// The name of the database engine in use.
pub(super) fn database_name(pool: &db::DbPool) -> &'static str {
    match pool {
        db::DbPool::Postgres(_) => "PostgreSQL",
        db::DbPool::Sqlite(_) => "SQLite",
    }
}

async fn show(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if ctx.administrator().is_none() {
        return ctx.error(StatusCode::FORBIDDEN, "Administrator access is required.");
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
