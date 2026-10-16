//! Admin storage overview page.
use crate::backend::api::context::PageContext;
use crate::backend::{app::AppState, db};
use crate::webui::{app::Page, pages::storage as view};
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

async fn show(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx
        .user
        .as_ref()
        .is_some_and(|user| user.is_admin && !user.is_banned && !user.must_change_password)
    {
        return ctx.error(StatusCode::FORBIDDEN, "Administrator access is required.");
    }
    let database = match ctx.state.pool {
        db::DbPool::Postgres(_) => "PostgreSQL",
        db::DbPool::Sqlite(_) => "SQLite",
    };
    let model = view::Page {
        database: database.into(),
        object_storage: match ctx.state.storage.kind() {
            "s3" => "S3-compatible bucket",
            _ => "Local directory",
        }
        .into(),
        location: ctx.state.storage.location(),
    };
    ctx.page_model_status(StatusCode::OK, "Storage", Page::Storage(model))
}
