//! Site administration routes; endpoint operations retain their current authority.
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_app::app::AppState;
use std::sync::Arc;
pub(crate) mod actions;
pub(crate) mod ai_crawlers;
pub(crate) mod audit;
pub(crate) mod email;
pub(crate) mod nodes;
pub(crate) mod overview;
pub(crate) mod signup_requests;
pub(crate) mod storage;
pub(crate) mod users;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route("/admin", get(crate::admin::overview::admin_get))
        .cell_route(
            "/admin/users",
            get(crate::admin::users::users_get).post(crate::admin::users::managed_user_post),
        )
        .cell_route("/admin/users/new", get(crate::admin::users::new_user_get))
        .cell_route(
            "/admin/users/requests/:request_id/resolve",
            post(crate::admin::users::signup_post),
        )
        .cell_route("/admin/audit", get(crate::admin::audit::audit_get))
        .cell_route(
            "/admin/settings",
            get(crate::admin::ai_crawlers::settings_get)
                .post(crate::admin::ai_crawlers::settings_post),
        )
        .cell_route(
            "/admin/accounts/:user_id/ban",
            post(crate::admin::users::ban_post),
        )
        .cell_route(
            "/admin/accounts/:user_id/role",
            post(crate::admin::users::role_post),
        )
        .cell_route(
            "/admin/accounts/:user_id/two-factor/reset",
            post(crate::admin::users::two_factor_reset_post),
        )
        .layer(DefaultBodyLimit::max(16 * 1024))
        .merge(crate::admin::actions::router())
        .merge(crate::admin::email::router())
        .merge(crate::admin::nodes::router())
        .merge(crate::admin::storage::router())
}
pub mod email_test;
