//! Site administration routes; endpoint operations retain their current authority.
pub(crate) mod accounts;
pub(crate) mod ai_crawlers;
pub(crate) mod audit;
pub(crate) mod email;
pub(crate) mod nodes;
pub(crate) mod overview;
pub(crate) mod signup_requests;
pub(crate) mod storage;
pub(crate) mod users;

use crate::backend::{
    api::{http::csrf, http::middleware},
    app::AppState,
};
use axum::{
    extract::DefaultBodyLimit,
    middleware::from_fn,
    routing::{get, patch, post},
    Router,
};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    // Administration JSON has the auth router's submission/cache policy, not
    // the ordinary API node-access policy. Each handler checks live admin access.
    let api = Router::new()
        .route("/api/auth/admin/users", post(users::admin_create_user))
        .route(
            "/api/auth/admin/accounts",
            get(accounts::admin_list_accounts),
        )
        .route(
            "/api/auth/admin/accounts/:user_id/ban",
            patch(accounts::admin_set_user_ban),
        )
        .route(
            "/api/auth/admin/signup-requests",
            get(signup_requests::admin_list_signup_requests),
        )
        .route(
            "/api/auth/admin/signup-requests/resolve",
            post(signup_requests::admin_resolve_signup_request),
        )
        .route(
            "/api/auth/admin/ai-crawlers",
            get(ai_crawlers::admin_get_ai_crawlers).put(ai_crawlers::admin_set_ai_crawlers),
        )
        .route_layer(from_fn(csrf::auth_submission))
        .layer(from_fn(middleware::private_api_response));
    let pages = Router::new()
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
        .layer(DefaultBodyLimit::max(16 * 1024));
    api.merge(pages)
        .merge(email::router())
        .merge(nodes::router())
        .merge(storage::router())
}
