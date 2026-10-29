//! Site administration routes; endpoint operations retain their current authority.
pub(crate) mod actions;
pub(crate) mod ai_crawlers;
pub(crate) mod audit;
pub(crate) mod email;
pub(crate) mod nodes;
pub(crate) mod overview;
pub(crate) mod signup_requests;
pub(crate) mod storage;
pub(crate) mod users;

use crate::backend::app::AppState;
use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin", get(overview::admin_get))
        .route(
            "/admin/users",
            get(users::users_get).post(users::managed_user_post),
        )
        .route("/admin/users/new", get(users::new_user_get))
        .route(
            "/admin/users/requests/:request_id/resolve",
            post(users::signup_post),
        )
        .route("/admin/audit", get(audit::audit_get))
        .route(
            "/admin/settings",
            get(ai_crawlers::settings_get).post(ai_crawlers::settings_post),
        )
        .route("/admin/accounts/:user_id/ban", post(users::ban_post))
        .route("/admin/accounts/:user_id/role", post(users::role_post))
        .route(
            "/admin/accounts/:user_id/two-factor/reset",
            post(users::two_factor_reset_post),
        )
        .layer(DefaultBodyLimit::max(16 * 1024))
        .merge(actions::router())
        .merge(email::router())
        .merge(nodes::router())
        .merge(storage::router())
}
