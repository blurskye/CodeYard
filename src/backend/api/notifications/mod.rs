pub mod done;
pub mod done_all;
pub mod inbox;
pub mod read;
pub mod read_all;
pub mod save;
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
            "/api/notifications/:notification_id/save",
            patch(save::set_notification_saved),
        )
        .route(
            "/api/notifications/:notification_id/done",
            patch(done::set_notification_done),
        )
        .route(
            "/api/notifications/read-all",
            post(read_all::mark_all_notifications_read),
        )
        .route(
            "/api/notifications/done-all",
            post(done_all::mark_all_notifications_done),
        )
        .route("/api/notifications/ws", get(ws::notifications_ws_handler))
        .route_layer(axum::middleware::from_fn(
            crate::backend::api::http::csrf::cookie_api_submission,
        ))
        .layer(axum::middleware::from_fn(
            crate::backend::api::http::middleware::private_sensitive_response,
        ));
    Router::new()
        .merge(json)
        .route("/notifications", get(inbox::notification_list))
        .route(
            "/notifications/:notification_id/read",
            post(read::notification_read),
        )
        .route(
            "/notifications/:notification_id/save",
            post(save::notification_save),
        )
        .route(
            "/notifications/:notification_id/done",
            post(done::notification_done),
        )
        .route(
            "/notifications/read-all",
            post(read_all::notification_read_all),
        )
        .route(
            "/notifications/done-all",
            post(done_all::notification_done_all),
        )
}
