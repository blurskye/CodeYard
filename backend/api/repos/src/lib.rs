#![recursion_limit = "256"]
#![allow(
    clippy::result_large_err,
    reason = "Page handlers return the original response error by value."
)]
//! Repository routes. The repository resource, its creation form, its settings page and the
//! repository-wide form endpoint stay here. Code, refs, settings and activity own their routes.
use axum::response::IntoResponse;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use codeyard_app::app::AppState;
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

pub fn router() -> Router<Arc<AppState>> {
    let json = Router::new()
        .route(
            "/api/repos",
            get(crate::collection::list_repos).post(crate::collection::create_repo),
        )
        .route(
            "/api/repos/:owner/:repo",
            get(crate::repository::get_repo)
                .patch(crate::repository::update_repo)
                .delete(crate::repository::delete_repo),
        )
        .route(
            "/api/repos/:owner/:repo/rename",
            post(crate::repository::rename_repo),
        )
        .route(
            "/api/repos/:owner/:repo/transfer",
            post(crate::repository::transfer_repo),
        )
        .route(
            "/api/repos/:owner/:repo/about",
            post(crate::repository::about_repo),
        )
        .merge(crate::code::json_router())
        .merge(crate::refs::json_router())
        .merge(crate::settings::json_router())
        .merge(crate::activity::json_router())
        .merge(crate::labels::router())
        .merge(crate::deploy_keys::json_router())
        .merge(crate::issues::json_router())
        .merge(crate::pulls::json_router())
        .merge(crate::releases::json_router())
        .route_layer(axum::middleware::from_fn(
            codeyard_api_http::interaction_proofs::prepare,
        ))
        .route_layer(axum::middleware::from_fn(
            codeyard_api_http::middleware::require_node_access,
        ))
        .route_layer(axum::middleware::from_fn(
            codeyard_api_http::csrf::cookie_api_submission,
        ))
        .layer(axum::middleware::from_fn(
            codeyard_api_http::middleware::private_api_response,
        ));
    let creation = Router::new()
        .route(
            "/new/migration",
            get(crate::migrations::new).post(crate::migrations::create),
        )
        .route(
            "/:owner/:repo/migration",
            get(crate::migrations::page).post(crate::migrations::submit),
        )
        .route(
            "/new",
            get(crate::collection::new_repository).post(crate::collection::create_repository),
        )
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024));
    Router::new()
        .merge(json)
        .merge(creation)
        .merge(crate::invitations::router())
        .merge(page_router())
        .nest(
            LEGACY_PREFIX,
            page_router()
                .merge(codeyard_api_http::shared::ci::repository_router())
                .layer(axum::middleware::from_fn(
                    codeyard_api_http::middleware::bounded_legacy_repository_body,
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
        .map(codeyard_webui::decode_component)
        .is_some_and(|owner| owner.eq_ignore_ascii_case("gists"))
    {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }
    *request.uri_mut() = uri;
    next.run(request).await
}
fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/settings", get(crate::repository::page))
        .route(
            "/:owner/:repo/_forms/:action",
            get(crate::repository::get_form).post(crate::repository::post_form),
        )
        .merge(crate::code::page_router())
        .merge(crate::refs::page_router())
        .merge(crate::releases::page_router())
        .merge(crate::insights::page_router())
        .merge(crate::security::page_router())
        .merge(crate::dependabot::page_router())
        .merge(crate::repository_discussions::page_router())
        .merge(crate::secret_scanning::page_router())
        .merge(crate::dependency_graph::page_router())
        .merge(crate::dependencies::page_router())
        .merge(crate::pages::page_router())
        .merge(crate::advisories::page_router())
        .merge(crate::disclosure::page_router())
        .merge(crate::checks::router())
        .layer(axum::extract::DefaultBodyLimit::max(
            codeyard_api_http::shared::repos::code::contents::EDITOR_FORM_BODY_LIMIT,
        ))
        .merge(crate::code::upload_router())
        .merge(crate::code::download_router())
        .merge(crate::refs::commits::router())
        .merge(crate::issues::page_router())
        .merge(crate::pulls::page_router())
        .merge(crate::milestones::page_router())
        .merge(crate::labels::page_router())
        .merge(crate::releases::upload_router())
        .layer(axum::middleware::from_fn(
            codeyard_api_http::interaction_proofs::prepare,
        ))
}

use codeyard_api_http::shared::repos::LEGACY_PREFIX;
