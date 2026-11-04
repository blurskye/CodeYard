//! Browser sign-in, account creation and native authentication forms.

pub mod change_password;
pub mod email_links;
pub(crate) mod errors;
mod github_apps;
pub mod login;
pub mod logout;
pub mod oauth;
mod oauth_apps;
pub mod session;
pub(crate) mod two_factor;

pub fn router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use axum::{
        crate::backend::api::http::middleware::from_fn, extract::DefaultBodyLimit, routing::get,
        Router,
    };
    Router::new()
        .merge(oauth_apps::router())
        .merge(github_apps::router())
        .merge(
            Router::new()
                .route("/enter", get(login::auth_get).post(login::auth_post))
                .route("/login", get(login::auth_get).post(login::auth_post))
                .merge(
                    Router::new()
                        .route("/login/oauth/:provider", get(oauth::oauth_redirect))
                        .route(
                            "/login/oauth/:provider/callback",
                            get(oauth::oauth_callback),
                        )
                        .layer(from_fn(
                            crate::backend::api::http::middleware::private_sensitive_response,
                        )),
                )
                .route(
                    "/login/two-factor",
                    get(two_factor::sign_in_get).post(two_factor::sign_in_post),
                )
                .route("/register", get(login::auth_get).post(login::auth_post))
                .route("/setup", get(login::auth_get).post(login::auth_post))
                .route(
                    "/forgot-password",
                    get(email_links::forgot_get).post(email_links::forgot_post),
                )
                .route("/logout", get(logout::logout_get).post(logout::logout_post))
                .merge(
                    // These addresses carry a token, so no cache keeps them.
                    Router::new()
                        .route(
                            "/reset-password",
                            get(email_links::reset_get).post(email_links::reset_post),
                        )
                        .route(
                            "/reset-password/code",
                            get(email_links::reset_code_get).post(email_links::reset_code_post),
                        )
                        .route(
                            "/verify-email",
                            get(email_links::verify_get).post(email_links::verify_post),
                        )
                        .layer(from_fn(
                            crate::backend::api::http::middleware::private_sensitive_response,
                        )),
                )
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
}
