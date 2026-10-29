//! GitHub Enterprise REST adapters. Domain operations keep their existing access checks.
mod action_permissions;
mod action_secrets;
mod action_variables;
mod actions;
mod activity;
mod advisories;
mod apps;
mod archives;
mod artifact_metadata;
mod assignees;
mod attestations;
pub(crate) mod auth;
mod autolinks;
mod blocks;
mod branch_operations;
mod branches;
mod catalog_search;
pub(in crate::backend::api) mod checks;
mod code_scanning;
mod code_security;
mod comments;
mod repository_attestations;
pub(crate) use code_scanning::{
    databases::{
        API_UPLOAD_ROUTE as CODEQL_API_UPLOAD_ROUTE, HOST_UPLOAD_ROUTE as CODEQL_HOST_UPLOAD_ROUTE,
        UPLOAD_ROUTE as CODEQL_UPLOAD_ROUTE,
    },
    SARIF_ACTION_ROUTE, SARIF_ROUTE,
};
mod code_owners;
mod collaborators;
mod commit_comments;
mod commit_pulls;
mod commits;
mod community;
mod comparison;
mod content_search;
mod contents;
mod copy;
mod dependabot;
mod dependabot_alerts;
mod dependencies;
mod dependency_graph;
mod deploy_keys;
mod deployment_protection;
mod deployments;
mod diff_media;
mod emails;
mod environment_values;
mod environments;
mod events;
mod gists;
mod git_objects;
mod global_issues;
mod gpg_keys;
mod issue_attributes;
pub(super) mod issues;
pub(crate) use issues::webhook_document as issue_webhook_document;
mod merges;
mod organization_values;
mod pages;
mod private_reporting;
mod rulesets;
pub(crate) use git_objects::BLOB_ROUTE;
mod immutable_releases;
mod interactions;
pub(super) mod labels;
pub(super) mod locking;
mod meta;
mod migrations;
mod milestones;
pub(crate) mod node_id;
mod optional_json;
mod orgs;
mod packages;
mod pagination;
mod pins;
mod private_registries;
mod projects;
mod protection;
mod protection_operations;
mod pull_caps;
mod pulls;
mod push_teams;
mod rate_limits;
pub(super) mod reactions;
mod refs;
mod relationships;
mod releases;
pub(super) mod repos;
mod repository_activity;
mod repository_teams;
mod routes;
mod runners;
pub(in crate::backend::api) mod search;
mod secret_scanning;
mod signing_keys;
mod social_accounts;
mod statistics;
mod statuses;
mod suggestions;
mod teams;
mod traffic;
mod users;
mod webhooks;

pub(crate) const RELEASE_ASSET_UPLOAD_ROUTE: &str =
    "/api/v3/repos/:owner/:repo/releases/:release_id/assets";
/// Gist writes read their own larger bodies (see `gists::buffered_write`).
pub(crate) const GISTS_ROUTE: &str = "/api/v3/gists";
pub(crate) const GIST_ROUTE: &str = "/api/v3/gists/:gist_id";

use crate::backend::{app::AppState, db};
use axum::{
    body::HttpBody,
    extract::{Path, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, on, MethodFilter, MethodRouter},
    Json, Router,
};
use routes::{Operation, Support, OPERATIONS};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug)]
pub(super) struct Error(StatusCode, String, Option<Value>);
pub(super) type Result<T> = std::result::Result<T, Error>;

/// Preserve Axum's transport/syntax/data classification inside the GitHub envelope.
pub(super) fn json_error(error: axum::extract::rejection::JsonRejection) -> Error {
    Error::new(error.status(), error.body_text())
}

pub(super) fn json_bytes_error(error: axum::Error) -> Error {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(error) = source {
        if error.is::<http_body_util::LengthLimitError>() { return Error::new(StatusCode::PAYLOAD_TOO_LARGE, "Request body is too large"); }
        source = error.source();
    }
    Error::new(StatusCode::BAD_REQUEST, "Failed to read request body")
}

/// Byte-buffered endpoints intentionally keep their optional-body and media-type policies.
pub(super) fn json_parse_error(error: serde_json::Error) -> Error {
    let status = if error.is_data() { StatusCode::UNPROCESSABLE_ENTITY } else { StatusCode::BAD_REQUEST };
    Error::new(status, error.to_string())
}

impl Error {
    pub(super) fn graphql(self) -> async_graphql::Error {
        let kind = match self.0 {
            StatusCode::NOT_FOUND => "NOT_FOUND",
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => "FORBIDDEN",
            StatusCode::SERVICE_UNAVAILABLE => "INTERNAL",
            _ => "UNPROCESSABLE",
        };
        super::graphql::error(self.1, kind)
    }

    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self(status, message.into(), None)
    }

    fn missing() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Not Found")
    }

    fn unavailable<E>(_: E) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "Service unavailable")
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, message)
    }

    fn invalid_simple(message: impl Into<String>) -> Self {
        Self::invalid(message).simple()
    }

    /// Lists a 422's message as a plain string, for operations GitHub documents with
    /// `validation-error-simple`.
    fn simple(mut self) -> Self {
        if self.0 == StatusCode::UNPROCESSABLE_ENTITY && self.2.is_none() {
            self.2 = Some(json!([self.1]));
        }
        self
    }

    fn validation(resource: &str, field: &str, code: &str) -> Self {
        Self(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Validation Failed".into(),
            Some(json!([{"resource": resource, "field": field, "code": code}])),
        )
    }

    fn response_with_docs(self, docs: &str) -> Response {
        let mut body = json!({
            "message": self.1,
            "documentation_url": docs,
            "status": self.0.as_u16().to_string(),
        });
        if let Some(errors) = self.2 {
            body["errors"] = errors;
        } else if self.0 == StatusCode::UNPROCESSABLE_ENTITY {
            body["errors"] = json!([{"code": "custom", "message": self.1}]);
        }
        if self.0 == StatusCode::CONFLICT {
            body["metadata"] = Value::Null;
        }
        (self.0, Json(body)).into_response()
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        self.response_with_docs("https://docs.github.com/en/rest")
    }
}

/// A pull request's diff or patch as plain text, for the `/pull/N.diff` and `.patch` pages.
pub(crate) async fn pull_diff_text(
    state: &AppState,
    repo: &db::models::Repository,
    pull: &db::models::PullRequest,
    patch: bool,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let media = if patch {
        diff_media::Media::Patch
    } else {
        diff_media::Media::Diff
    };
    pulls::diff_response(state, repo, pull, media)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

pub(crate) fn api_url(path: &str) -> String {
    format!("{}/api/v3{path}", super::http::config::link_origin())
}

pub(crate) fn repo_url(owner: &str, repo: &str) -> String {
    api_url(&format!(
        "/repos/{}/{}",
        crate::webui::encode_component(owner),
        crate::webui::encode_component(repo),
    ))
}

/// Non-authoritative identity lookup for derived Git proof preparation. The actual handler
/// still owns authentication, token scope, rate limits and the final domain authority check.
pub(crate) async fn interaction_actor(
    state: &AppState,
    headers: &HeaderMap,
) -> Option<db::models::User> {
    let credential = auth::credential(headers)?;
    let token = db::users::tokens::find_token(
        &state.pool,
        &crate::backend::identity::tokens::token_hash(credential),
    )
    .await
    .ok()??;
    db::users::tokens::token_user(&state.pool, token.id, false)
        .await
        .ok()?
}

pub(super) async fn repository(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    write: bool,
) -> Result<(db::models::Repository, Option<auth::Authentication>)> {
    let auth = auth::authenticate(state, headers, write).await?;
    if write {
        auth.as_ref().ok_or_else(auth::required)?.require_write()?;
    }
    let repo = db::repositories::records::get_repository(&state.pool, owner, name)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    if !crate::backend::repositories::access::can_read(
        &state.pool,
        auth.as_ref().map(|auth| &auth.user),
        &repo,
    )
    .await
    .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    Ok((repo, auth))
}

async fn get_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    Ok(Json(
        repos::document(&state, &repo, auth.as_ref()).await?,
    ))
}

async fn missing() -> Error {
    Error::missing()
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
    response.headers_mut().insert(
        "x-github-api-version-selected",
        "2022-11-28".parse().unwrap(),
    );
    response
}

/// The handlers each area implements, by GitHub operation ID.
fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = vec![("repos/get", get(get_repository))];
    handlers.extend(actions::handlers());
    handlers.extend(repository_attestations::handlers());
    handlers.extend(action_permissions::handlers());
    handlers.extend(action_secrets::handlers());
    handlers.extend(action_variables::handlers());
    handlers.extend(organization_values::handlers());
    handlers.extend(environment_values::handlers());
    handlers.extend(apps::handlers());
    handlers.extend(users::handlers());
    handlers.extend(orgs::handlers());
    handlers.extend(emails::handlers());
    handlers.extend(environments::handlers());
    handlers.extend(deployments::handlers());
    handlers.extend(deployment_protection::handlers());
    handlers.extend(blocks::handlers());
    handlers.extend(gpg_keys::handlers());
    handlers.extend(gists::handlers());
    handlers.extend(interactions::handlers());
    handlers.extend(pull_caps::handlers());
    handlers.extend(signing_keys::handlers());
    handlers.extend(social_accounts::handlers());
    handlers.extend(branches::handlers());
    handlers.extend(checks::handlers());
    handlers.extend(rulesets::handlers());
    handlers.extend(branch_operations::handlers());
    handlers.extend(comments::handlers());
    handlers.extend(code_scanning::handlers());
    handlers.extend(code_security::handlers());
    handlers.extend(commit_comments::handlers());
    handlers.extend(collaborators::handlers());
    handlers.extend(code_owners::handlers());
    handlers.extend(community::handlers());
    handlers.extend(commits::handlers());
    handlers.extend(activity::handlers());
    handlers.extend(archives::handlers());
    handlers.extend(autolinks::handlers());
    handlers.extend(comparison::handlers());
    handlers.extend(merges::handlers());
    handlers.extend(immutable_releases::handlers());
    handlers.extend(dependencies::handlers());
    handlers.extend(contents::handlers());
    handlers.extend(copy::handlers());
    handlers.extend(deploy_keys::handlers());
    handlers.extend(dependabot::handlers());
    handlers.extend(dependabot_alerts::handlers());
    handlers.extend(dependency_graph::handlers());
    handlers.extend(pages::handlers());
    handlers.extend(advisories::handlers());
    handlers.extend(private_reporting::handlers());
    handlers.extend(events::handlers());
    handlers.extend(git_objects::handlers());
    handlers.extend(assignees::handlers());
    handlers.extend(artifact_metadata::handlers());
    handlers.extend(attestations::handlers());
    handlers.extend(issues::handlers());
    handlers.extend(issue_attributes::handlers());
    handlers.extend(suggestions::handlers());
    handlers.extend(labels::handlers());
    handlers.extend(locking::handlers());
    handlers.extend(search::handlers());
    handlers.extend(catalog_search::handlers());
    handlers.extend(content_search::handlers());
    handlers.extend(secret_scanning::handlers());
    handlers.extend(global_issues::handlers());
    handlers.extend(pins::handlers());
    handlers.extend(relationships::handlers());
    handlers.extend(milestones::handlers());
    handlers.extend(meta::handlers());
    handlers.extend(packages::handlers());
    handlers.extend(migrations::handlers());
    handlers.extend(reactions::handlers());
    handlers.extend(rate_limits::handlers());
    handlers.extend(pulls::handlers());
    handlers.extend(projects::handlers());
    handlers.extend(private_registries::handlers());
    handlers.extend(protection::handlers());
    handlers.extend(protection_operations::handlers());
    handlers.extend(push_teams::handlers());
    handlers.extend(commit_pulls::handlers());
    handlers.extend(refs::handlers());
    handlers.extend(repos::handlers());
    handlers.extend(repository_teams::handlers());
    handlers.extend(repository_activity::handlers());
    handlers.extend(releases::handlers());
    handlers.extend(statistics::handlers());
    handlers.extend(traffic::handlers());
    handlers.extend(statuses::handlers());
    handlers.extend(teams::handlers());
    handlers.extend(runners::handlers());
    handlers.extend(webhooks::handlers());
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
        .merge(archives::aliases())
        .route(
            "/advisories",
            axum::routing::post(dependabot_alerts::import),
        )
        .merge(code_scanning::aliases())
        .merge(actions::aliases())
        .merge(contents::aliases())
        .merge(environment_values::aliases())
        .merge(gists::aliases())
        .merge(repository_attestations::aliases())
        .merge(refs::aliases())
        .merge(migrations::aliases())
        .merge(meta::aliases())
        .fallback(missing);
    Router::new()
        .nest("/api/v3", api)
        .route("/api/v3/", get(meta::root))
        .route(
            CODEQL_UPLOAD_ROUTE,
            axum::routing::post(code_scanning::databases::upload)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            CODEQL_HOST_UPLOAD_ROUTE,
            axum::routing::post(code_scanning::databases::upload)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .layer(axum::middleware::from_fn(auth::authorize_request))
        .layer(axum::middleware::from_fn(rate_limits::enforce))
        .layer(axum::middleware::from_fn(
            super::http::middleware::bounded_form_body,
        ))
        .layer(axum::middleware::from_fn(responses))
        .merge(activity::feeds::resources())
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_operation_routes() {
        // Building the router panics on conflicting paths and on handlers for unknown operations.
        let _ = super::router();
    }
}
