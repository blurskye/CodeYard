//! Public account profiles and follow forms.

pub mod activity;
pub mod avatar;
pub mod follow;
pub mod profile;

pub fn router() -> axum::Router<std::sync::Arc<codeyard_app::app::AppState>> {
    use axum::extract::DefaultBodyLimit;
    use axum::middleware::from_fn;
    use axum::routing::get;
    use axum::routing::post;
    use axum::Router;
    Router::new()
        .merge(
            Router::new()
                .route("/avatars/:name", get(crate::users::avatar::avatar_get))
                .route_layer(from_fn(codeyard_api_http::middleware::require_node_access))
                .layer(from_fn(codeyard_api_http::middleware::private_api_response)),
        )
        .route("/:username", get(crate::users::profile::profile_get))
        .route(
            "/gists/@users/:username",
            get(crate::users::profile::profile_get),
        )
        .route(
            "/gists/@users/:username/_forms/follow",
            post(crate::users::follow::follow_post),
        )
        .route(
            "/:username/_forms/follow",
            post(crate::users::follow::follow_post),
        )
        .layer(DefaultBodyLimit::max(64 * 1024))
}
