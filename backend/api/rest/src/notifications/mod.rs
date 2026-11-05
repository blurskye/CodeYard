use codeyard_api_http::cell_routes::CellRoutes;
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
        .cell_route(
            "/api/notifications",
            get(crate::notifications::inbox::list_notifications),
        )
        .cell_route(
            "/api/notifications/:notification_id/read",
            patch(crate::notifications::read::mark_notification_read),
        )
        .cell_route(
            "/api/notifications/:notification_id/save",
            patch(crate::notifications::save::set_notification_saved),
        )
        .cell_route(
            "/api/notifications/:notification_id/done",
            patch(crate::notifications::done::set_notification_done),
        )
        .cell_route(
            "/api/notifications/read-all",
            post(crate::notifications::read_all::mark_all_notifications_read),
        )
        .cell_route(
            "/api/notifications/done-all",
            post(crate::notifications::done_all::mark_all_notifications_done),
        )
        .cell_route(
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
        .cell_route(
            "/notifications",
            get(crate::notifications::inbox::notification_list),
        )
        .cell_route(
            "/notifications/:notification_id/read",
            post(crate::notifications::read::notification_read),
        )
        .cell_route(
            "/notifications/:notification_id/save",
            post(crate::notifications::save::notification_save),
        )
        .cell_route(
            "/notifications/:notification_id/done",
            post(crate::notifications::done::notification_done),
        )
        .cell_route(
            "/notifications/read-all",
            post(crate::notifications::read_all::notification_read_all),
        )
        .cell_route(
            "/notifications/done-all",
            post(crate::notifications::done_all::notification_done_all),
        )
}
