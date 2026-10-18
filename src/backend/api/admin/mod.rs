//! Site administration routes; endpoint operations retain their current authority.
pub(crate) mod accounts;
pub(crate) mod activity_chart;
pub(crate) mod ai_crawlers;
pub(crate) mod nodes;
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
            "/api/auth/admin/activity-chart",
            get(activity_chart::admin_activity_chart),
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
        .route("/admin", get(accounts::admin_get))
        // The account list is on the overview; this is the address people try first.
        .route(
            "/admin/users",
            get(|| async { crate::backend::api::http::render::redirect_response("/admin") }),
        )
        .route("/admin/access/users", post(users::managed_user_post))
        .route("/admin/ai-crawlers", post(ai_crawlers::ai_crawlers_post))
        .route(
            "/admin/access/requests/:request_id/resolve",
            post(signup_requests::signup_post),
        )
        // The Account access tab in Settings posts to the same handlers.
        .route("/settings/access/users", post(users::managed_user_post))
        .route(
            "/settings/access/requests/:request_id/resolve",
            post(signup_requests::signup_post),
        )
        .route(
            "/admin/accounts/:user_id/ban",
            get(accounts::ban_get).post(accounts::ban_post),
        )
        .layer(DefaultBodyLimit::max(16 * 1024));
    api.merge(pages)
        .merge(nodes::router())
        .merge(storage::router())
}
