#![recursion_limit = "256"]
#![allow(clippy::allow_attributes_without_reason, clippy::result_large_err)]
//! GitHub Enterprise REST adapters. Domain operations keep their existing access checks.
use crate::routes::Operation;
use crate::routes::Support;
use crate::routes::OPERATIONS;
use axum::body::HttpBody;
use axum::extract::Request;
use axum::http::header;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::on;
use axum::routing::MethodFilter;
use axum::routing::MethodRouter;
use axum::Router;
use codeyard_api_http::shared::github::code_scanning::databases::HOST_UPLOAD_ROUTE as CODEQL_HOST_UPLOAD_ROUTE;
use codeyard_api_http::shared::github::code_scanning::databases::UPLOAD_ROUTE as CODEQL_UPLOAD_ROUTE;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use std::collections::HashMap;
use std::sync::Arc;
mod actions;
mod activity;
mod apps;
pub mod auth;
pub mod checks;
#[path = "code-scanning/mod.rs"]
mod code_scanning;
#[path = "code-security/mod.rs"]
mod code_security;

mod dependabot;
#[path = "dependency-graph/mod.rs"]
mod dependency_graph;
mod diff_media;
mod gists;
pub mod issues;

mod interactions;
mod meta;
mod migrations;
mod optional_json;
mod orgs;
mod packages;
mod pagination;
#[path = "private-registries/mod.rs"]
mod private_registries;
mod projects;
mod pulls;
pub mod reactions;
pub mod repos;
mod routes;
pub mod search;
#[path = "secret-scanning/mod.rs"]
mod secret_scanning;
mod teams;
mod users;

/// A pull request's diff or patch as plain text, for the `/pull/N.diff` and `.patch` pages.
pub async fn pull_diff_text(
    state: &AppState,
    repo: &codeyard_db_core::models::Repository,
    pull: &codeyard_db_core::models::PullRequest,
    patch: bool,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let media = if patch {
        crate::diff_media::Media::Patch
    } else {
        crate::diff_media::Media::Diff
    };
    crate::pulls::diff_response(state, repo, pull, media)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

pub(crate) fn empty(status: StatusCode) -> Response {
    (status, [(header::CONTENT_LENGTH, "0")]).into_response()
}

// Extractor and routing failures use GitHub errors; probes may explicitly return an empty 404.
async fn responses(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    if (response.status().is_client_error() || response.status().is_server_error())
        // Existence checks use a bodyless 404, as GitHub documents.
        && !(matches!(response.status(),StatusCode::NOT_FOUND | StatusCode::CONFLICT) && response.body().size_hint().exact() == Some(0))
        && response
            .headers()
            .get(header::CONTENT_LENGTH)
            .is_none_or(|value| value != "0")
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            != Some("application/json")
    {
        let status = response.status();
        response = Error::new(
            status,
            status.canonical_reason().unwrap_or("Request failed"),
        )
        .into_response();
    }
    response
        .headers_mut()
        .entry(header::CACHE_CONTROL)
        .or_insert_with(|| "no-store".parse().unwrap());
    response
        .headers_mut()
        .append(header::VARY, "Accept".parse().unwrap());
    response.headers_mut().insert(
        "x-github-api-version-selected",
        "2022-11-28".parse().unwrap(),
    );
    response
}

/// The handlers each area implements, by GitHub operation ID.
fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = vec![("repos/get", get(get_repository))];
    handlers.extend(crate::actions::handlers());
    handlers.extend(crate::repos::repository_attestations::handlers());
    handlers.extend(crate::actions::action_permissions::handlers());
    handlers.extend(crate::actions::action_secrets::handlers());
    handlers.extend(crate::actions::action_variables::handlers());
    handlers.extend(crate::actions::organization_values::handlers());
    handlers.extend(crate::actions::environment_values::handlers());
    handlers.extend(crate::apps::handlers());
    handlers.extend(crate::users::handlers());
    handlers.extend(crate::orgs::handlers());
    handlers.extend(crate::users::emails::handlers());
    handlers.extend(crate::repos::environments::handlers());
    handlers.extend(crate::repos::deployments::handlers());
    handlers.extend(crate::repos::deployment_protection::handlers());
    handlers.extend(crate::users::blocks::handlers());
    handlers.extend(crate::users::gpg_keys::handlers());
    handlers.extend(crate::gists::handlers());
    handlers.extend(crate::interactions::handlers());
    handlers.extend(crate::interactions::pull_caps::handlers());
    handlers.extend(crate::users::signing_keys::handlers());
    handlers.extend(crate::users::social_accounts::handlers());
    handlers.extend(crate::repos::branches::handlers());
    handlers.extend(crate::checks::handlers());
    handlers.extend(crate::repos::rulesets::handlers());
    handlers.extend(crate::repos::branch_operations::handlers());
    handlers.extend(crate::issues::comments::handlers());
    handlers.extend(crate::code_scanning::handlers());
    handlers.extend(crate::code_security::handlers());
    handlers.extend(crate::repos::commit_comments::handlers());
    handlers.extend(crate::repos::collaborators::handlers());
    handlers.extend(crate::repos::code_owners::handlers());
    handlers.extend(crate::repos::community::handlers());
    handlers.extend(crate::repos::commits::handlers());
    handlers.extend(crate::activity::handlers());
    handlers.extend(crate::repos::archives::handlers());
    handlers.extend(crate::repos::autolinks::handlers());
    handlers.extend(crate::repos::comparison::handlers());
    handlers.extend(crate::repos::merges::handlers());
    handlers.extend(crate::repos::immutable_releases::handlers());
    handlers.extend(crate::repos::dependencies::handlers());
    handlers.extend(crate::repos::contents::handlers());
    handlers.extend(crate::repos::copy::handlers());
    handlers.extend(crate::repos::deploy_keys::handlers());
    handlers.extend(crate::dependabot::handlers());
    handlers.extend(crate::dependabot::dependabot_alerts::handlers());
    handlers.extend(crate::dependency_graph::handlers());
    handlers.extend(crate::repos::pages::handlers());
    handlers.extend(crate::security_advisories::advisories::handlers());
    handlers.extend(crate::repos::private_reporting::handlers());
    handlers.extend(crate::issues::events::handlers());
    handlers.extend(crate::git::git_objects::handlers());
    handlers.extend(crate::issues::assignees::handlers());
    handlers.extend(crate::orgs::artifact_metadata::handlers());
    handlers.extend(crate::orgs::attestations::handlers());
    handlers.extend(crate::issues::handlers());
    handlers.extend(crate::issues::issue_attributes::handlers());
    handlers.extend(crate::issues::suggestions::handlers());
    handlers.extend(crate::issues::labels::handlers());
    handlers.extend(crate::issues::locking::handlers());
    handlers.extend(crate::search::handlers());
    handlers.extend(crate::search::catalog_search::handlers());
    handlers.extend(crate::search::content_search::handlers());
    handlers.extend(crate::secret_scanning::handlers());
    handlers.extend(crate::issues::global_issues::handlers());
    handlers.extend(crate::issues::pins::handlers());
    handlers.extend(crate::issues::relationships::handlers());
    handlers.extend(crate::issues::milestones::handlers());
    handlers.extend(crate::meta::handlers());
    handlers.extend(crate::packages::handlers());
    handlers.extend(crate::migrations::handlers());
    handlers.extend(crate::reactions::handlers());
    handlers.extend(crate::rate_limit::rate_limits::handlers());
    handlers.extend(crate::pulls::handlers());
    handlers.extend(crate::projects::handlers());
    handlers.extend(crate::private_registries::handlers());
    handlers.extend(crate::repos::protection::handlers());
    handlers.extend(crate::repos::protection_operations::handlers());
    handlers.extend(crate::repos::push_teams::handlers());
    handlers.extend(crate::repos::commit_pulls::handlers());
    handlers.extend(crate::git::refs::handlers());
    handlers.extend(crate::repos::handlers());
    handlers.extend(crate::repos::repository_teams::handlers());
    handlers.extend(crate::repos::repository_activity::handlers());
    handlers.extend(crate::repos::releases::handlers());
    handlers.extend(crate::repos::statistics::handlers());
    handlers.extend(crate::repos::traffic::handlers());
    handlers.extend(crate::repos::statuses::handlers());
    handlers.extend(crate::teams::handlers());
    handlers.extend(crate::actions::runners::handlers());
    handlers.extend(crate::repos::webhooks::handlers());
    handlers
}

/// Answers an operation without a handler: 501 while it is still to do, 404 when Codeyard does
/// not support it, as GitHub Enterprise Server answers for a feature it lacks.
fn placeholder(operation: &'static Operation) -> MethodRouter<Arc<AppState>> {
    let filter = MethodFilter::try_from(operation.method.clone()).expect("a REST method");
    on(filter, move || async move {
        let (status, message) = match operation.support {
            Support::Supported => (
                StatusCode::NOT_IMPLEMENTED,
                "Codeyard does not implement this endpoint yet",
            ),
            Support::NotSupported => (StatusCode::NOT_FOUND, "Not Found"),
        };
        Error::new(status, message).response_with_docs(operation.docs)
    })
}

/// Every operation in GitHub's description gets a route, with its handler or a placeholder.
fn operations() -> Router<Arc<AppState>> {
    let mut handlers: HashMap<_, _> = handlers().into_iter().collect();
    let mut router = Router::new();
    for operation in OPERATIONS {
        let route = match handlers.remove(operation.id) {
            Some(route) if matches!(operation.support, Support::Supported) => route,
            Some(_) => panic!("{} is not supported but has a handler", operation.id),
            None => placeholder(operation),
        };
        router = router.route(operation.path, route);
    }
    let unknown: Vec<_> = handlers.keys().collect();
    assert!(
        unknown.is_empty(),
        "handlers for unknown operations: {unknown:?}"
    );
    router
}

pub fn router() -> Router<Arc<AppState>> {
    let api = operations()
        .merge(crate::repos::archives::aliases())
        .route(
            "/advisories",
            axum::routing::post(crate::dependabot::dependabot_alerts::import),
        )
        .merge(crate::code_scanning::aliases())
        .merge(crate::actions::aliases())
        .merge(crate::repos::contents::aliases())
        .merge(crate::actions::environment_values::aliases())
        .merge(crate::gists::aliases())
        .merge(crate::repos::repository_attestations::aliases())
        .merge(crate::git::refs::aliases())
        .merge(crate::migrations::aliases())
        .merge(crate::meta::aliases())
        .fallback(missing);
    Router::new()
        .nest("/api/v3", api)
        .route("/api/v3/", get(crate::meta::root))
        .route(
            CODEQL_UPLOAD_ROUTE,
            axum::routing::post(crate::code_scanning::databases::upload)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            CODEQL_HOST_UPLOAD_ROUTE,
            axum::routing::post(crate::code_scanning::databases::upload)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .layer(axum::middleware::from_fn(
            codeyard_api_http::interaction_proofs::prepare,
        ))
        .layer(axum::middleware::from_fn(crate::auth::authorize_request))
        .layer(axum::middleware::from_fn(
            crate::rate_limit::rate_limits::enforce,
        ))
        .layer(axum::middleware::from_fn(crate::auth::request_context))
        .layer(axum::middleware::from_fn(
            codeyard_api_http::middleware::bounded_form_body,
        ))
        .layer(axum::middleware::from_fn(responses))
        .merge(crate::activity::feeds::resources())
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_operation_routes() {
        // Building the router panics on conflicting paths and on handlers for unknown operations.
        let _ = crate::router();
    }
}

use codeyard_app::api_shared::github::get_repository;

use codeyard_app::api_shared::github::missing;

pub mod git;

#[path = "rate-limit/mod.rs"]
pub mod rate_limit;

#[path = "security-advisories/mod.rs"]
pub mod security_advisories;
