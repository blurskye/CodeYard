//! Authentication endpoints and their browser aliases. Administration has its own router.

pub mod change_password;
pub(crate) mod errors;
pub mod login;
pub mod logout;
pub mod me;
pub mod oauth;
pub mod register;
pub mod session;
pub mod setup;

pub fn router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use crate::backend::api::{csrf, middleware};
    use axum::{
        extract::DefaultBodyLimit,
        middleware::from_fn,
        routing::{get, post},
        Router,
    };
    Router::new()
        .merge(
            Router::new()
                .route("/api/auth/session", post(session::create_session))
                .route("/api/auth/setup-status", get(setup::setup_status))
                .route("/api/auth/setup", post(setup::setup))
                .route("/api/auth/register", post(register::register))
                .route("/api/auth/login", post(login::login))
                .route("/api/auth/logout", post(logout::logout))
                .route("/api/auth/me", get(me::me))
                .route("/api/auth/oauth/:provider", get(oauth::oauth_redirect))
                .route(
                    "/api/auth/oauth/:provider/callback",
                    get(oauth::oauth_callback),
                )
                .route(
                    "/api/auth/change-password",
                    post(change_password::change_password),
                )
                .route_layer(from_fn(csrf::auth_submission))
                .layer(from_fn(middleware::private_api_response)),
        )
        .merge(
            Router::new()
                .route("/enter", get(login::auth_get).post(login::auth_post))
                .route("/login", get(login::auth_get).post(login::auth_post))
                .route("/register", get(login::auth_get).post(login::auth_post))
                .route("/setup", get(login::auth_get).post(login::auth_post))
                .route("/logout", get(logout::logout_get).post(logout::logout_post))
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
}
