//! Repository routes. The repository resource, its creation form, its settings page and the
//! repository-wide form endpoint stay here. Code, refs, settings and activity own their routes.
use crate::backend::app::AppState;
use axum::{
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use std::sync::Arc;

pub mod access;
pub mod activity;
pub mod advisories;
pub mod checks;
pub mod chrome;
pub mod code;
pub mod collection;
pub mod dependabot;
pub mod dependencies;
pub mod dependency_graph;
pub mod deploy_keys;
pub mod disclosure;
pub mod discussion;
pub mod forms;
pub mod insights;
mod invitations;
pub mod issues;
pub mod labels;
mod migrations;
pub mod milestones;
pub mod pages;
pub mod paging;
mod properties;
pub mod pulls;
pub mod refs;
pub mod releases;
pub mod repository;
mod repository_discussions;
pub mod responses;
pub mod secret_scanning;
pub mod security;
pub mod settings;

pub(crate) const LEGACY_PREFIX: &str = "/gists/@repos";

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
            super::http::interaction_proofs::prepare,
        ))
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
            "/new/migration",
            get(migrations::new).post(migrations::create),
        )
        .route(
            "/:owner/:repo/migration",
            get(migrations::page).post(migrations::submit),
        )
        .route(
            "/new",
            get(collection::new_repository).post(collection::create_repository),
        )
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024));
    Router::new()
        .merge(json)
        .merge(creation)
        .merge(invitations::router())
        .merge(page_router())
        .nest(
            LEGACY_PREFIX,
            page_router()
                .merge(super::ci::repository_router())
                .layer(axum::middleware::from_fn(
                    super::http::middleware::bounded_legacy_repository_body,
                ))
                .layer(axum::middleware::from_fn(legacy_alias_uri)),
        )
}

// Nested handlers need their public URI for native forms and hydrated navigation.
async fn legacy_alias_uri(
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if !uri
        .path()
        .split('/')
        .nth(3)
        .map(crate::webui::decode_component)
        .is_some_and(|owner| owner.eq_ignore_ascii_case("gists"))
    {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }
    *request.uri_mut() = uri;
    next.run(request).await
}
fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/settings", get(repository::page))
        .route(
            "/:owner/:repo/_forms/:action",
            get(repository::get_form).post(repository::post_form),
        )
        .merge(code::page_router())
        .merge(refs::page_router())
        .merge(releases::page_router())
        .merge(insights::page_router())
        .merge(security::page_router())
        .merge(dependabot::page_router())
        .merge(repository_discussions::page_router())
        .merge(secret_scanning::page_router())
        .merge(dependency_graph::page_router())
        .merge(dependencies::page_router())
        .merge(pages::page_router())
        .merge(advisories::page_router())
        .merge(disclosure::page_router())
        .merge(checks::router())
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024))
        .merge(code::upload_router())
        .merge(code::download_router())
        .merge(refs::commits::router())
        .merge(issues::page_router())
        .merge(pulls::page_router())
        .merge(milestones::page_router())
        .merge(labels::page_router())
        .merge(releases::upload_router())
        .layer(axum::middleware::from_fn(super::http::interaction_proofs::prepare))
}
