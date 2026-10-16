//! Signed-in account resources, with JSON and native forms kept distinct.

mod appearance;
pub mod avatar;
pub mod keys;
pub mod profile;
pub mod responses;
pub mod tokens;

/// A rejected settings form is shown at its POST-only URL. A GET there, from a reload, a
/// copied address or a typed one, goes to the settings tab the form belongs to.
fn back_to_settings(
    tab: &'static str,
) -> axum::routing::MethodRouter<std::sync::Arc<crate::backend::app::AppState>> {
    axum::routing::get(move || async move {
        crate::backend::api::render::redirect_response(&format!("/settings?tab={tab}"))
    })
}

pub fn router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use crate::backend::api::{auth::change_password, csrf, middleware};
    use axum::{
        extract::DefaultBodyLimit,
        middleware::from_fn,
        routing::{delete, get, patch, post, put},
        Router,
    };
    Router::new()
        .merge(
            Router::new()
                .route("/api/user", patch(profile::update_user_profile))
                .route(
                    "/api/user/avatar",
                    put(avatar::set_avatar).delete(avatar::remove_avatar),
                )
                .route(
                    "/api/user/keys",
                    get(keys::list_ssh_keys).post(keys::add_ssh_key),
                )
                .route("/api/user/keys/:key_id", delete(keys::delete_ssh_key))
                .merge(
                    Router::new()
                        .route(
                            "/api/user/tokens",
                            get(tokens::list_tokens).post(tokens::create_token),
                        )
                        .route("/api/user/tokens/:token_id", delete(tokens::delete_token))
                        .layer(from_fn(middleware::private_sensitive_response)),
                )
                .route_layer(from_fn(middleware::require_node_access))
                .route_layer(from_fn(csrf::cookie_api_submission))
                .layer(from_fn(middleware::private_api_response)),
        )
        .merge(
            Router::new()
                .route("/_forms/theme", post(appearance::set_theme))
                .route("/settings/appearance", back_to_settings("appearance"))
                .route("/settings", get(profile::settings_get))
                .route(
                    "/settings/profile",
                    back_to_settings("profile").post(profile::profile_post),
                )
                .route(
                    "/settings/password",
                    back_to_settings("password").post(change_password::password_post),
                )
                .route(
                    "/settings/keys",
                    back_to_settings("keys").post(keys::key_add_post),
                )
                .route(
                    "/settings/keys/:key_id/delete",
                    back_to_settings("keys").post(keys::key_delete_post),
                )
                .route(
                    "/settings/tokens",
                    back_to_settings("tokens").post(tokens::token_add_post),
                )
                .route(
                    "/settings/tokens/:token_id/delete",
                    back_to_settings("tokens").post(tokens::token_delete_post),
                )
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .merge(avatar::router())
}
