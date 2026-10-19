//! Repository routes. The repository resource, its creation form, its settings page and the
//! repository-wide form endpoint stay here. Code, refs, settings and activity own their routes.
use crate::backend::app::AppState;
use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;

pub mod access;
pub mod activity;
pub mod chrome;
pub mod code;
pub mod collection;
pub mod deploy_keys;
pub mod issues;
pub mod labels;
pub mod milestones;
pub mod pulls;
pub mod refs;
pub mod releases;
pub mod repository;
pub mod responses;
pub mod settings;

pub fn router() -> Router<Arc<AppState>> {
    let json = Router::new()
        .route(
            "/api/repos",
            get(collection::list_repos).post(collection::create_repo),
        )
        .route(
            "/api/repos/:owner/:repo",
            get(repository::get_repo)
                .patch(repository::update_repo)
                .delete(repository::delete_repo),
        )
        .route(
            "/api/repos/:owner/:repo/rename",
            post(repository::rename_repo),
        )
        .route(
            "/api/repos/:owner/:repo/transfer",
            post(repository::transfer_repo),
        )
        .route(
            "/api/repos/:owner/:repo/about",
            post(repository::about_repo),
        )
        .merge(code::json_router())
        .merge(refs::json_router())
        .merge(settings::json_router())
        .merge(activity::json_router())
        .merge(labels::router())
        .merge(deploy_keys::json_router())
        .merge(issues::json_router())
        .merge(pulls::json_router())
        .merge(releases::json_router())
        .route_layer(axum::middleware::from_fn(
            crate::backend::api::http::middleware::require_node_access,
        ))
        .route_layer(axum::middleware::from_fn(
            crate::backend::api::http::csrf::cookie_api_submission,
        ))
        .layer(axum::middleware::from_fn(
            crate::backend::api::http::middleware::private_api_response,
        ));
    let creation = Router::new()
        .route(
            "/new",
            get(collection::new_repository).post(collection::create_repository),
        )
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024));
    let pages = Router::new()
        .route("/:owner/:repo/settings", get(repository::page))
        .route(
            "/:owner/:repo/_forms/:action",
            get(repository::get_form).post(repository::post_form),
        )
        .merge(code::page_router())
        .merge(refs::page_router())
        .merge(releases::page_router())
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024));
    Router::new()
        .merge(json)
        .merge(creation)
        .merge(pages)
        .merge(code::upload_router())
        .merge(code::download_router())
        .merge(refs::commits::router())
        .merge(issues::page_router())
        .merge(pulls::page_router())
        .merge(milestones::page_router())
        .merge(labels::page_router())
        .merge(releases::upload_router())
}
