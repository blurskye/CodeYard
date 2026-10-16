pub mod inbox;
pub mod read;
pub mod read_all;
pub mod ws;

pub fn router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use axum::{
        routing::{get, patch, post},
        Router,
    };
    let json = Router::new()
        .route("/api/notifications", get(inbox::list_notifications))
        .route(
            "/api/notifications/:notification_id/read",
            patch(read::mark_notification_read),
        )
        .route(
            "/api/notifications/read-all",
            post(read_all::mark_all_notifications_read),
        )
        .route("/api/notifications/ws", get(ws::notifications_ws_handler))
        .layer(axum::middleware::from_fn(
            crate::backend::api::middleware::private_sensitive_response,
        ));
    Router::new()
        .merge(json)
        .route("/notifications", get(inbox::notification_list))
        .route(
            "/notifications/:notification_id/read",
            post(read::notification_read),
        )
        .route(
            "/notifications/read-all",
            post(read_all::notification_read_all),
        )
}
