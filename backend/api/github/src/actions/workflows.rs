//! Workflows are discovered from the default branch; IDs and disabled state are durable.
use axum::response::IntoResponse;
use codeyard_api_http::cell_routes::CellRoutes;

use axum::extract::rejection::JsonRejection;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use axum::Router;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_ci_engine::runs::DispatchRequest;
use codeyard_db_actions::action_workflows as workflows;
use codeyard_db_actions::action_workflows::Workflow;
use codeyard_db_core::ci_admission::AdmissionError;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::repo_url;
use codeyard_repos_core::api_shared::github::contents::store_error;
use codeyard_repos_core::repositories::store::repo_store;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(super) async fn discover(
    state: &AppState,
    repo: &codeyard_db_core::models::Repository,
) -> Result<Vec<Workflow>> {
    let store = repo_store(repo, state.cluster.as_ref());
    if store.refs().await.map_err(store_error)?.is_empty() {
        return Ok(Vec::new());
    }
    let definitions = codeyard_repos_core::workflows::list_workflows_for_ref(
        &store,
        &format!("refs/heads/{}", repo.default_branch),
    )
    .await
    .map_err(Error::unavailable)?;
    let definitions: Vec<_> = definitions
        .into_iter()
        .map(|definition| (definition.path, definition.name))
        .collect();
    workflows::register_workflows(&state.pool, repo.id, &definitions, true)
        .await
        .map_err(Error::unavailable)
}

pub(super) async fn lookup(
    state: &AppState,
    repo: &codeyard_db_core::models::Repository,
    key: &str,
) -> Result<Workflow> {
    discover(state, repo)
        .await?
        .into_iter()
        .find(|workflow| {
            key.parse::<i64>().ok() == Some(workflow.id)
                || workflow.path == key
                || workflow.path.strip_prefix(".github/workflows/") == Some(key)
        })
        .ok_or_else(Error::missing)
}

pub(crate) fn admission_error(error: AdmissionError) -> Error {
    match error {
        AdmissionError::WorkflowDisabled(message) => Error::new(StatusCode::CONFLICT, message),
        AdmissionError::Missing(_) => Error::missing(),
        AdmissionError::Denied(message) => Error::new(StatusCode::FORBIDDEN, message),
        AdmissionError::Conflict("CI ref unavailable") => {
            Error::invalid("No ref found for this reference")
        }
        AdmissionError::Conflict(message) => Error::new(StatusCode::CONFLICT, message),
        AdmissionError::Invalid(message) | AdmissionError::InvalidRequest(message) => {
            Error::invalid(message)
        }
        AdmissionError::Unavailable => Error::unavailable(()),
    }
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let workflows = discover(&state, &repo).await?;
    let total = workflows.len();
    let (offset, limit) = page.window()?;
    let values: Vec<_> = workflows
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|workflow| codeyard_webhooks::payloads::action_workflow(&workflow, &owner, &name))
        .collect();
    Ok((
        page.headers(&uri, offset + limit < total)?,
        Json(json!({"total_count":total,"workflows":values})),
    )
        .into_response())
}

async fn get_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, key)): Path<(String, String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    Ok(Json(codeyard_webhooks::payloads::action_workflow(
        &lookup_read(&state, &repo, &key).await?,
        &owner,
        &name,
    )))
}

pub(super) async fn lookup_read(
    state: &AppState,
    repo: &codeyard_db_core::models::Repository,
    key: &str,
) -> Result<workflows::Workflow> {
    match lookup(state, repo, key).await {
        Ok(workflow) => Ok(workflow),
        Err(error) if error.0 == StatusCode::NOT_FOUND => {
            let id = key.parse::<i64>().map_err(|_| Error::missing())?;
            let mut workflow = workflows::workflow_by_id(&state.pool, repo.id, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            workflow.state = "deleted".into();
            Ok(workflow)
        }
        Err(error) => Err(error),
    }
}

async fn enabled(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    key: &str,
    enable: bool,
) -> Result<StatusCode> {
    let (repo, authentication) = repository(state, headers, owner, name, true).await?;
    let authentication = authentication.ok_or_else(codeyard_identity::github_auth::required)?;
    let workflow = lookup(state, &repo, key).await?;
    workflows::set_workflow_enabled(
        &state.pool,
        authentication.database_actor(),
        repo.id,
        workflow.id,
        enable,
    )
    .await
    .map_err(|error| {
        if error
            .downcast_ref::<codeyard_db_repos::repositories::records::RepositorySettingsRefusal>()
            .is_some()
        {
            Error::new(StatusCode::FORBIDDEN, "Workflow update denied")
        } else {
            Error::unavailable(error)
        }
    })?
    .ok_or_else(Error::missing)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn enable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, key)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    enabled(&state, &headers, &owner, &name, &key, true).await
}
async fn disable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, key)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    enabled(&state, &headers, &owner, &name, &key, false).await
}

#[derive(Deserialize)]
struct Dispatch {
    #[serde(rename = "ref")]
    reference: String,
    inputs: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    return_run_details: bool,
}

async fn dispatch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, key)): Path<(String, String, String)>,
    input: std::result::Result<Json<Dispatch>, JsonRejection>,
) -> Result<Response> {
    let (repo, authentication) = repository(&state, &headers, &owner, &name, true).await?;
    let authentication = authentication.ok_or_else(codeyard_identity::github_auth::required)?;
    let credential = authentication.ci_credential(&headers, "actions")?;
    let actor = authentication.workflow_actor(&state, repo.id).await?;
    let Json(input) = input.map_err(|_| Error::invalid("Invalid workflow dispatch"))?;
    let workflow = lookup(&state, &repo, &key).await?;
    let run = state
        .ci_runs
        .dispatch_run(
            actor,
            DispatchRequest {
                repo_owner: owner.clone(),
                repo_name: name.clone(),
                branch: input.reference,
                event: "workflow_dispatch".to_owned(),
                head_sha: None,
                workflow_path: Some(workflow.path),
                pipeline: None,
                environment: None,
                inputs: input.inputs,
                repo_id: Some(repo.id),
            },
            Some(credential),
        )
        .await
        .map_err(admission_error)?;
    if input.return_run_details {
        return Ok(Json(json!({"workflow_run_id":run.id,"run_url":format!("{}/actions/runs/{}",repo_url(&owner,&name),run.id),"html_url":format!("{}/{}/{}/actions?job={}",codeyard_model::http_config::link_origin(),codeyard_types::paths::encode_component(&owner),codeyard_types::paths::encode_component(&name),run.run_id)})).into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct BadgeQuery {
    branch: Option<String>,
    event: Option<String>,
}

async fn badge(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, key)): Path<(String, String, String)>,
    Query(query): Query<BadgeQuery>,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let workflow = lookup(&state, &repo, &key).await?;
    let branch = codeyard_db_core::ci_messages::branch_name(
        query.branch.as_deref().unwrap_or(&repo.default_branch),
    );
    let latest = workflows::latest_workflow_run(
        &state.pool,
        repo.id,
        &workflow.path,
        Some(branch),
        query.event.as_deref(),
    )
    .await
    .map_err(Error::unavailable)?;
    let (status, color) = if workflow.state != "active" {
        ("disabled", "#9f9f9f")
    } else {
        match latest.as_ref().map(|run| run.status.as_str()) {
            Some("success") => ("passing", "#4c1"),
            Some("failed") => ("failing", "#e05d44"),
            Some("running") => ("in progress", "#dfb317"),
            Some("queued") => ("queued", "#9f9f9f"),
            Some("cancelled") => ("cancelled", "#9f9f9f"),
            _ => ("no status", "#9f9f9f"),
        }
    };
    let escape = codeyard_webui::components::escape_html;
    let label = escape(&workflow.name.chars().take(22).collect::<String>());
    let title = escape(&workflow.name);
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="240" height="20" role="img" aria-label="{label}: {status}"><title>{title}: {status}</title><rect width="150" height="20" fill="#555"/><rect x="150" width="90" height="20" fill="{color}"/><g fill="#fff" text-anchor="middle" font-family="Verdana,sans-serif" font-size="11"><text x="75" y="14">{label}</text><text x="195" y="14">{status}</text></g></svg>"##
    );
    Ok((
        [(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")],
        svg,
    )
        .into_response())
}

pub(super) fn aliases() -> Router<Arc<AppState>> {
    Router::new().cell_route(
        "/repos/:owner/:repo/actions/workflows/:workflow_id/badge.svg",
        get(badge),
    )
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/list-repo-workflows", get(list)),
        ("actions/get-workflow", get(get_one)),
        ("actions/enable-workflow", put(enable)),
        ("actions/disable-workflow", put(disable)),
        ("actions/create-workflow-dispatch", post(dispatch)),
    ]
}
