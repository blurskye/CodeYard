//! Public account profiles, repositories and follow resources.

pub mod activity;
pub mod avatar;
pub mod follow;
pub mod profile;
pub mod repos;

pub fn router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use crate::backend::api::{http::csrf, http::middleware};
    use axum::{extract::DefaultBodyLimit, middleware::from_fn, routing::get, Router};
    Router::new()
        .merge(
            Router::new()
                .route("/api/users/:username", get(profile::get_user))
                .route("/api/users/:username/repos", get(repos::list_user_repos))
                .route(
                    "/api/users/:username/activity",
                    get(activity::list_user_activity),
                )
                .route(
                    "/api/users/:username/follow",
                    get(follow::check_user_follow)
                        .put(follow::follow_user)
                        .delete(follow::unfollow_user),
                )
                .route_layer(from_fn(middleware::require_node_access))
                .route_layer(from_fn(csrf::cookie_api_submission))
                .layer(from_fn(middleware::private_api_response)),
        )
        .merge(
            Router::new()
                .merge(
                    Router::new()
                        .route("/avatars/:name", get(avatar::avatar_get))
                        .route_layer(from_fn(middleware::require_node_access))
                        .layer(from_fn(middleware::private_api_response)),
                )
                .route("/:username", get(profile::profile_get))
                .route(
                    "/:username/_forms/follow",
                    axum::routing::post(follow::follow_post),
                )
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
}
