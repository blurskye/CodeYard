pub mod done;
pub mod done_all;
pub mod inbox;
pub mod read;
pub mod read_all;
pub mod save;
pub mod ws;

pub fn router() -> axum::Router<std::sync::Arc<codeyard_app::app::AppState>> {
    use axum::{
        routing::{get, patch, post},
        Router,
    };
    let json = Router::new()
        .route(
            "/api/notifications",
            get(crate::notifications::inbox::list_notifications),
        )
        .route(
            "/api/notifications/:notification_id/read",
            patch(crate::notifications::read::mark_notification_read),
        )
        .route(
            "/api/notifications/:notification_id/save",
            patch(crate::notifications::save::set_notification_saved),
        )
        .route(
            "/api/notifications/:notification_id/done",
            patch(crate::notifications::done::set_notification_done),
        )
        .route(
            "/api/notifications/read-all",
            post(crate::notifications::read_all::mark_all_notifications_read),
        )
        .route(
            "/api/notifications/done-all",
            post(crate::notifications::done_all::mark_all_notifications_done),
        )
        .route(
            "/api/v3/notifications/ws",
            get(crate::notifications::ws::notifications_ws_handler),
        )
        .route_layer(axum::middleware::from_fn(
            codeyard_api_http::csrf::cookie_api_submission,
        ))
        .layer(axum::middleware::from_fn(
            codeyard_api_http::middleware::private_sensitive_response,
        ));
    Router::new()
        .merge(json)
        .route(
            "/notifications",
            get(crate::notifications::inbox::notification_list),
        )
        .route(
            "/notifications/:notification_id/read",
            post(crate::notifications::read::notification_read),
        )
        .route(
            "/notifications/:notification_id/save",
            post(crate::notifications::save::notification_save),
        )
        .route(
            "/notifications/:notification_id/done",
            post(crate::notifications::done::notification_done),
        )
        .route(
            "/notifications/read-all",
            post(crate::notifications::read_all::notification_read_all),
        )
        .route(
            "/notifications/done-all",
            post(crate::notifications::done_all::notification_done_all),
        )
}
