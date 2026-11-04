//! Deployment requests use immutable commits and preserve every reported status.
use axum::extract::FromRequest;
use axum::response::IntoResponse;

use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::Request;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_repos_core::api_shared::github::contents::store_error;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_repos_pulls::repositories::merges::BranchMerge;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_db_actions::deployments::Deployment;
use codeyard_db_actions::deployments::Filter;
use codeyard_db_actions::deployments::NewDeployment;
use codeyard_db_actions::deployments::NewStatus;
use codeyard_db_actions::deployments::Refusal;
use codeyard_db_actions::deployments::Status;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::repo_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

fn write_error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => Error::new(StatusCode::FORBIDDEN, "Deployment write denied"),
        Some(Refusal::NotFound) => Error::missing(),
        Some(Refusal::Checks) => Error::new(
            StatusCode::CONFLICT,
            "Required commit status checks have not succeeded",
        ),
        Some(Refusal::Active) => Error::invalid_simple("An active deployment cannot be deleted"),
        Some(Refusal::Invalid) => Error::validation("Deployment", "configuration", "invalid"),
        None => Error::unavailable(error),
    }
}

async fn creator(state: &AppState, id: Option<i64>) -> Result<Option<Value>> {
    match id {
        Some(id) => Ok(
            codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .map(|user| codeyard_webhooks::payloads::user(&user)),
        ),
        None => Ok(None),
    }
}

pub(crate) async fn document(
    state: &AppState,
    deployment: &Deployment,
    owner: &str,
    repo: &str,
) -> Result<Value> {
    Ok(codeyard_webhooks::payloads::deployment(
        deployment,
        creator(state, deployment.creator_id).await?,
        serde_json::from_str(&deployment.payload).map_err(Error::unavailable)?,
        owner,
        repo,
    ))
}

async fn status_document(
    state: &AppState,
    status: &Status,
    owner: &str,
    repo: &str,
) -> Result<Value> {
    Ok(codeyard_webhooks::payloads::deployment_status(
        status,
        creator(state, status.creator_id).await?,
        owner,
        repo,
    ))
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(page): Query<Pagination>,
    Query(filter): Query<Filter>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let (offset, limit) = page.window()?;
    let rows = codeyard_db_actions::deployments::list(
        &state.pool,
        auth.as_ref().map(|actor| actor.user.id),
        repo.id,
        &filter,
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = rows.len() > limit;
    let mut values = Vec::new();
    for row in rows.into_iter().take(limit) {
        values.push(document(&state, &row, &owner, &name).await?);
    }
    page.response(&uri, values, has_next)
}

#[derive(Deserialize)]
struct Create {
    #[serde(rename = "ref")]
    reference: String,
    task: Option<String>,
    auto_merge: Option<bool>,
    required_contexts: Option<Vec<String>>,
    payload: Option<Value>,
    environment: Option<String>,
    description: Option<String>,
    transient_environment: Option<bool>,
    production_environment: Option<bool>,
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    request: Request,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let actor = &auth.user;
    codeyard_identity::repository_access::check_write(&state.pool, repo.id, actor.id)
        .await
        .map_err(|error| match error {
            codeyard_identity::repository_access::AccessError::Unavailable => {
                Error::unavailable(())
            }
            _ => Error::new(StatusCode::FORBIDDEN, "Deployment write denied"),
        })?;
    let Json(input) = Json::<Create>::from_request(request, &state)
        .await
        .map_err(|_| Error::validation("Deployment", "ref", "invalid"))?;
    let store = repo_store(&repo, state.cluster.as_ref());
    let sha = store
        .commit_from_primary(&input.reference)
        .await
        .map_err(|error| {
            if matches!(
                error.kind,
                codeyard_git::error::ErrorKind::NotFound | codeyard_git::error::ErrorKind::Invalid
            ) {
                Error::validation("Deployment", "ref", "invalid")
            } else {
                store_error(error)
            }
        })?
        .sha;
    let payload = input.payload.unwrap_or_else(|| json!({}));
    let environment = input.environment.as_deref().unwrap_or("production");
    let new = NewDeployment {
        actor: auth.database_actor(),
        repo: repo.id,
        sha: &sha,
        reference: &input.reference,
        task: input.task.as_deref().unwrap_or("deploy"),
        payload: &payload,
        environment,
        description: input.description.as_deref(),
        transient: input.transient_environment.unwrap_or(false),
        production: input
            .production_environment
            .unwrap_or(environment.eq_ignore_ascii_case("production")),
        required_contexts: input.required_contexts.as_deref(),
    };
    new.validate().map_err(write_error)?;
    if input.auto_merge.unwrap_or(true) {
        let refs = store.refs_from_primary().await.map_err(store_error)?;
        let branch = input
            .reference
            .strip_prefix("refs/heads/")
            .unwrap_or(&input.reference);
        if refs.get(&format!("refs/heads/{branch}")) == Some(&sha) {
            let default = refs
                .get(&format!("refs/heads/{}", repo.default_branch))
                .ok_or_else(|| Error::new(StatusCode::CONFLICT, "Default branch not found"))?;
            if !store
                .is_ancestor(default, &sha)
                .await
                .map_err(store_error)?
            {
                // The merge moves the branch like a direct merge, so the credential needs
                // contents write, and publication checks its workflows permission.
                auth.repository_scope(&state.pool, repo.id, "contents", true)
                    .await
                    .map_err(crate::auth::scope_error)?;
                codeyard_repos_pulls::repositories::merges::merge_branch(
                    &state.pool,
                    state.cluster.as_ref(),
                    &state.ci_runs,
                    auth.database_actor(),
                    repo.id,
                    &owner,
                    BranchMerge {
                        base: branch,
                        head: &repo.default_branch,
                        message: None,
                        expected_base: Some(&sha),
                        expected_head: Some(default),
                    },
                )
                .await
                .map_err(crate::repos::merges::failure)?;
                return Ok((StatusCode::ACCEPTED, Json(json!({"message": format!("Auto-merged {} into {branch} on deployment.", repo.default_branch)}))).into_response());
            }
        }
    }
    let saved = codeyard_db_actions::deployments::create(&state.pool, new)
        .await
        .map_err(write_error)?;
    let value = document(&state, &saved, &owner, &name).await?;
    let deployment = value.clone();
    let snapshot = repo.clone();
    let sender = codeyard_webhooks::payloads::Person::of(actor);
    codeyard_repos_core::webhooks::emit(&state.pool, &repo, codeyard_model::webhook_events::DEPLOYMENT, vec![actor.id], move |owner, _| json!({"deployment": deployment, "repository": codeyard_webhooks::payloads::repository(&snapshot, owner), "sender": codeyard_webhooks::payloads::person(&sender)})).await;
    Ok((
        StatusCode::CREATED,
        [(
            header::LOCATION,
            format!("{}/deployments/{}", repo_url(&owner, &name), saved.id),
        )],
        Json(value),
    )
        .into_response())
}

async fn deployment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    method: Method,
) -> Result<Response> {
    let write = !crate::auth::read_method(&method);
    let (repo, auth) = repository(&state, &headers, &owner, &name, write).await?;
    if write {
        codeyard_db_actions::deployments::delete(
            &state.pool,
            auth.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
                .database_actor(),
            repo.id,
            id,
        )
        .await
        .map_err(write_error)?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let saved = codeyard_db_actions::deployments::get(
        &state.pool,
        auth.as_ref().map(|actor| actor.user.id),
        repo.id,
        id,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(Error::missing)?;
    Ok(Json(document(&state, &saved, &owner, &name).await?).into_response())
}

async fn statuses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Result<Response> {
    let read = crate::auth::read_method(request.method());
    let (owner, name) = (&params["owner"], &params["repo"]);
    let (repo, auth) = repository(&state, &headers, owner, name, !read).await?;
    let viewer = auth.as_ref().map(|actor| actor.user.id);
    let id = params["deployment_id"]
        .parse()
        .map_err(|_| Error::missing())?;
    let saved = codeyard_db_actions::deployments::get(&state.pool, viewer, repo.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    if read {
        if let Some(status) = params.get("status_id") {
            let row = codeyard_db_actions::deployments::get_status(
                &state.pool,
                viewer,
                id,
                status.parse().map_err(|_| Error::missing())?,
            )
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
            return Ok(Json(status_document(&state, &row, owner, name).await?).into_response());
        }
        let (offset, limit) = page.window()?;
        let rows = codeyard_db_actions::deployments::statuses(
            &state.pool,
            viewer,
            id,
            offset as i64,
            (limit + 1) as i64,
        )
        .await
        .map_err(Error::unavailable)?;
        let has_next = rows.len() > limit;
        let mut values = Vec::new();
        for row in rows.into_iter().take(limit) {
            values.push(status_document(&state, &row, owner, name).await?);
        }
        return page.response(&uri, values, has_next);
    }
    let authentication = auth.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let credential = authentication.database_actor();
    let actor = authentication.user;
    let Json(input) = Json::<NewStatus>::from_request(request, &state)
        .await
        .map_err(|_| Error::validation("DeploymentStatus", "state", "invalid"))?;
    let statuses = codeyard_db_actions::deployments::create_status(
        &state.pool,
        credential,
        repo.id,
        saved.id,
        &input,
    )
    .await
    .map_err(write_error)?;
    let mut result = None;
    for (deployment, status) in statuses {
        let value = status_document(&state, &status, owner, name).await?;
        if status.deployment_id == saved.id {
            result = Some(value.clone());
        }
        let deployment = document(&state, &deployment, owner, name).await?;
        let snapshot = repo.clone();
        let sender = codeyard_webhooks::payloads::Person::of(&actor);
        codeyard_repos_core::webhooks::emit(&state.pool, &repo, codeyard_model::webhook_events::DEPLOYMENT_STATUS, vec![actor.id], move |owner, _| json!({"deployment_status": value, "deployment": deployment, "repository": codeyard_webhooks::payloads::repository(&snapshot, owner), "sender": codeyard_webhooks::payloads::person(&sender)})).await;
    }
    let value = result.ok_or_else(|| Error::unavailable(()))?;
    Ok((
        StatusCode::CREATED,
        [(
            header::LOCATION,
            value["url"]
                .as_str()
                .ok_or_else(|| Error::unavailable(()))?
                .to_owned(),
        )],
        Json(value),
    )
        .into_response())
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-deployments", get(list)),
        ("repos/create-deployment", post(create)),
        ("repos/get-deployment", get(deployment)),
        ("repos/delete-deployment", axum::routing::delete(deployment)),
        ("repos/list-deployment-statuses", get(statuses)),
        ("repos/get-deployment-status", get(statuses)),
        ("repos/create-deployment-status", post(statuses)),
    ]
}
