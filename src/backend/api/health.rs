//! Liveness and database readiness for load balancers.
use crate::backend::app::AppState;
use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({ "status": "ok" })) }),
        )
        .route("/ready", get(ready))
}

async fn ready(State(state): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
    match tokio::time::timeout(std::time::Duration::from_secs(2), state.pool.ping()).await {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "ready" })),
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "status": "database unavailable" })),
        ),
    }
}
