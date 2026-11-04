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

pub fn router() -> axum::Router<std::sync::Arc<codeyard_app::app::AppState>> {
    use axum::codeyard_api_http::middleware::from_fn;
    use axum::extract::DefaultBodyLimit;
    use axum::routing::get;
    use axum::Router;
    Router::new()
        .merge(crate::auth::oauth_apps::router())
        .merge(crate::auth::github_apps::router())
        .merge(
            Router::new()
                .route(
                    "/enter",
                    get(crate::auth::login::auth_get).post(crate::auth::login::auth_post),
                )
                .route(
                    "/login",
                    get(crate::auth::login::auth_get).post(crate::auth::login::auth_post),
                )
                .merge(
                    Router::new()
                        .route(
                            "/login/oauth/:provider",
                            get(crate::auth::oauth::oauth_redirect),
                        )
                        .route(
                            "/login/oauth/:provider/callback",
                            get(crate::auth::oauth::oauth_callback),
                        )
                        .layer(from_fn(
                            codeyard_api_http::middleware::private_sensitive_response,
                        )),
                )
                .route(
                    "/login/two-factor",
                    get(crate::auth::two_factor::sign_in_get)
                        .post(crate::auth::two_factor::sign_in_post),
                )
                .route(
                    "/register",
                    get(crate::auth::login::auth_get).post(crate::auth::login::auth_post),
                )
                .route(
                    "/setup",
                    get(crate::auth::login::auth_get).post(crate::auth::login::auth_post),
                )
                .route(
                    "/forgot-password",
                    get(crate::auth::email_links::forgot_get)
                        .post(crate::auth::email_links::forgot_post),
                )
                .route(
                    "/logout",
                    get(crate::auth::logout::logout_get).post(crate::auth::logout::logout_post),
                )
                .merge(
                    // These addresses carry a token, so no cache keeps them.
                    Router::new()
                        .route(
                            "/reset-password",
                            get(crate::auth::email_links::reset_get)
                                .post(crate::auth::email_links::reset_post),
                        )
                        .route(
                            "/reset-password/code",
                            get(crate::auth::email_links::reset_code_get)
                                .post(crate::auth::email_links::reset_code_post),
                        )
                        .route(
                            "/verify-email",
                            get(crate::auth::email_links::verify_get)
                                .post(crate::auth::email_links::verify_post),
                        )
                        .layer(from_fn(
                            codeyard_api_http::middleware::private_sensitive_response,
                        )),
                )
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
}
