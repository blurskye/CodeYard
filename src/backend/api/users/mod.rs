//! Public account profiles and follow forms.

pub mod activity;
pub mod avatar;
pub mod follow;
pub mod profile;

pub fn router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use axum::{
        crate::backend::api::http::middleware::from_fn,
        extract::DefaultBodyLimit,
        routing::{get, post},
        Router,
    };
    Router::new()
        .merge(
            Router::new()
                .route("/avatars/:name", get(avatar::avatar_get))
                .route_layer(from_fn(
                    crate::backend::api::http::middleware::require_node_access,
                ))
                .layer(from_fn(
                    crate::backend::api::http::middleware::private_api_response,
                )),
        )
        .route("/:username", get(profile::profile_get))
        .route("/gists/@users/:username", get(profile::profile_get))
        .route(
            "/gists/@users/:username/_forms/follow",
            post(follow::follow_post),
        )
        .route("/:username/_forms/follow", post(follow::follow_post))
        .layer(DefaultBodyLimit::max(64 * 1024))
}
