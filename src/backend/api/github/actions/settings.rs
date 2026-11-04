//! Settings and policy documents are stored with the native organization and repository policy.
use codeyard_model::urls::api_url;

use crate::backend::api::github::pagination::Pagination;
use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;
use codeyard_model::urls::repo_url;

use axum::{
    extract::{OriginalUri, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put, MethodRouter},
    Json,
};
use codeyard_db_actions::action_settings::Policy;
use codeyard_db_core::queries::action_settings::Refusal;
use codeyard_db_core::queries::action_settings::Scope;
use codeyard_db_core::queries::action_settings::Setting;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

type Params = HashMap<String, String>;

pub(super) fn failure(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Actions administration required")
        }
        Some(Refusal::Missing) => Error::missing(),
        Some(Refusal::Invalid) => Error::validation("Actions", "configuration", "invalid"),
        Some(Refusal::Conflict) => {
            Error::new(StatusCode::CONFLICT, "Deployment is no longer pending")
        }
        None => Error::unavailable(error),
    }
}

async fn scope(
    state: &AppState,
    headers: &HeaderMap,
    params: &Params,
    write: bool,
) -> Result<(Scope, crate::backend::api::github::auth::Authentication)> {
    if let Some(org) = params.get("org") {
        let auth = crate::backend::api::github::auth::authenticate(state, headers, write)
            .await?
            .ok_or_else(crate::backend::api::github::auth::required)?;
        if write {
            auth.require_write()?;
        }
        Ok((Scope::Organization(org.clone()), auth))
    } else {
        let (repo, auth) =
            repository(state, headers, &params["owner"], &params["repo"], write).await?;
        Ok((
            Scope::Repository(repo.id),
            auth.ok_or_else(crate::backend::api::github::auth::required)?,
        ))
    }
}

async fn input(request: Request) -> Result<Value> {
    let bytes = axum::body::to_bytes(request.into_body(), 65536)
        .await
        .map_err(|_| Error::invalid("Invalid Actions configuration"))?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::validation("Actions", "configuration", "invalid"))
}

async fn settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Result<Response> {
    let write = !crate::backend::api::github::auth::read_method(request.method());
    let (scope, auth) = scope(&state, &headers, &params, write).await?;
    let actor = auth.database_actor();
    let setting = match uri.path().rsplit('/').next() {
        Some("selected-actions") => Setting::Allowed,
        Some("artifact-and-log-retention") => Setting::Retention,
        Some("fork-pr-contributor-approval") => Setting::ContributorApproval,
        Some("workflow") => Setting::Workflow,
        Some("fork-pr-workflows-private-repos") => Setting::ForkWorkflows,
        Some("access") => Setting::Access,
        _ => return Err(Error::missing()),
    };
    let body = if write {
        Some(input(request).await?)
    } else {
        None
    };
    let stored = codeyard_db_actions::action_settings::settings(
        &state.pool,
        actor,
        &scope,
        setting,
        body.as_ref(),
    )
    .await
    .map_err(failure)?;
    if write {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(Json(
        stored
            .get(setting.key())
            .cloned()
            .unwrap_or_else(|| setting.defaults()),
    )
    .into_response())
}

/// The organization switches for Actions and for self-hosted runners. Their selected
/// repositories are stored with the switch and apply only while it is `selected`.
fn switch(uri: &Uri) -> Setting {
    if uri.path().contains("/self-hosted-runners") {
        Setting::Runners
    } else {
        Setting::Permissions
    }
}

fn switch_url(org: &str, setting: Setting, rest: &str) -> String {
    let path = match setting {
        Setting::Runners => "permissions/self-hosted-runners",
        _ => "permissions",
    };
    api_url(&format!(
        "/orgs/{}/actions/{path}{rest}",
        codeyard_webui::encode_component(org)
    ))
}

async fn organization_switch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Result<Response> {
    let write = !crate::backend::api::github::auth::read_method(request.method());
    let (scope, auth) = scope(&state, &headers, &params, write).await?;
    let actor = auth.database_actor();
    let setting = switch(&uri);
    if write {
        let body = input(request).await?;
        // The repository list has its own endpoints, which check ownership.
        if body.get("enabled_repositories").is_none()
            || body.get("selected_repository_ids").is_some()
        {
            return Err(Error::validation(
                "Actions",
                "enabled_repositories",
                "invalid",
            ));
        }
        codeyard_db_actions::action_settings::settings(
            &state.pool,
            actor,
            &scope,
            setting,
            Some(&body),
        )
        .await
        .map_err(failure)?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let stored =
        codeyard_db_actions::action_settings::settings(&state.pool, actor, &scope, setting, None)
            .await
            .map_err(failure)?;
    let mut value = setting.defaults();
    if let (Some(value), Some(stored)) = (value.as_object_mut(), stored[setting.key()].as_object())
    {
        for (key, field) in stored {
            if key != "selected_repository_ids" {
                value.insert(key.clone(), field.clone());
            }
        }
    }
    let org = &params["org"];
    if matches!(setting, Setting::Permissions) {
        value["selected_actions_url"] = json!(switch_url(org, setting, "/selected-actions"));
    }
    if value["enabled_repositories"] == "selected" {
        value["selected_repositories_url"] = json!(switch_url(org, setting, "/repositories"));
    }
    Ok(Json(value).into_response())
}

async fn organization_repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Result<Response> {
    let method = request.method().clone();
    let write = !crate::backend::api::github::auth::read_method(&method);
    let (scope, auth) = scope(&state, &headers, &params, write).await?;
    let actor = auth.database_actor();
    let setting = switch(&uri);
    use codeyard_db_core::queries::action_settings::SelectionChange;
    let change = if !write {
        None
    } else if params.contains_key("repository_id") {
        let id = crate::backend::api::github::actions::id(&params, "repository_id")?;
        Some(if method == axum::http::Method::DELETE {
            SelectionChange::Remove(id)
        } else {
            SelectionChange::Add(id)
        })
    } else {
        let body = input(request).await?;
        let ids = body["selected_repository_ids"]
            .as_array()
            .ok_or_else(|| Error::validation("Actions", "selected_repository_ids", "missing"))?
            .iter()
            .map(|id| {
                id.as_i64().ok_or_else(|| {
                    Error::validation("Actions", "selected_repository_ids", "invalid")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Some(SelectionChange::Set(ids))
    };
    let (offset, limit) = page.window()?;
    let (total, rows) = codeyard_db_actions::action_settings::repositories(
        &state.pool,
        actor,
        &scope,
        setting,
        change.as_ref(),
        (offset as i64, limit as i64),
    )
    .await
    .map_err(failure)?;
    if write {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let values: Vec<_> = rows
        .iter()
        .map(|repo| crate::backend::webhooks::payloads::repository(repo, &params["org"]))
        .collect();
    Ok((
        page.headers(&uri, (offset.saturating_add(limit) as i64) < total)?,
        Json(json!({"total_count":total,"repositories":values})),
    )
        .into_response())
}

fn document(policy: &Policy, params: &Params, organization: Option<&str>) -> Result<Value> {
    let mut value: Value = serde_json::from_str(&policy.data).map_err(Error::unavailable)?;
    let (source_type, source, url) =
        if let Some(org) = organization.or_else(|| params.get("org").map(String::as_str)) {
            (
                "Organization",
                org.to_owned(),
                api_url(&format!(
                    "/orgs/{}/actions/policies/{}",
                    codeyard_webui::encode_component(org),
                    policy.id
                )),
            )
        } else {
            (
                "Repository",
                format!("{}/{}", params["owner"], params["repo"]),
                format!(
                    "{}/actions/policies/{}",
                    repo_url(&params["owner"], &params["repo"]),
                    policy.id
                ),
            )
        };
    value["id"] = json!(policy.id);
    value["node_id"] = json!(crate::backend::api::github::node_id::encode(
        "AP", policy.id
    ));
    value["target"] = json!("actions");
    value["source_type"] = json!(source_type);
    value["source"] = json!(source);
    value["created_at"] = json!(policy.created_at);
    value["updated_at"] = json!(policy.updated_at);
    value["_links"] = json!({"self":{"href":url}});
    Ok(value)
}

#[derive(serde::Deserialize)]
struct PolicyQuery {
    has_parents: Option<bool>,
}

async fn policies(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    Query(query): Query<PolicyQuery>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Result<Response> {
    let method = request.method().clone();
    let write = !crate::backend::api::github::auth::read_method(&method);
    let (scope, auth) = scope(&state, &headers, &params, write).await?;
    let actor = auth.database_actor();
    let id = params
        .get("policy_id")
        .map(|_| crate::backend::api::github::actions::id(&params, "policy_id"))
        .transpose()?;
    if !write && id.is_none() {
        let (offset, limit) = page.window()?;
        let (total, rows) = codeyard_db_actions::action_settings::policies_page(
            &state.pool,
            actor,
            &scope,
            query.has_parents.unwrap_or(true),
            offset as i64,
            limit as i64,
        )
        .await
        .map_err(failure)?;
        let values = rows
            .iter()
            .map(|row| document(&row.policy, &params, row.organization.as_deref()))
            .collect::<Result<Vec<_>>>()?;
        return Ok((
            page.headers(&uri, (offset.saturating_add(limit) as i64) < total)?,
            Json(json!({"total_count":total,"policies":values})),
        )
            .into_response());
    }
    let body = if write && method != axum::http::Method::DELETE {
        Some(input(request).await?)
    } else {
        None
    };
    let change = if write { Some(body.as_ref()) } else { None };
    let rows =
        codeyard_db_actions::action_settings::policies(&state.pool, actor, &scope, id, change)
            .await
            .map_err(failure)?;
    if method == axum::http::Method::DELETE {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let value = document(rows.first().ok_or_else(Error::missing)?, &params, None)?;
    Ok((
        if id.is_none() {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(value),
    )
        .into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/get-allowed-actions-organization", get(settings)),
        ("actions/set-allowed-actions-organization", put(settings)),
        ("actions/get-allowed-actions-repository", get(settings)),
        ("actions/set-allowed-actions-repository", put(settings)),
        (
            "actions/get-artifact-and-log-retention-settings-organization",
            get(settings),
        ),
        (
            "actions/set-artifact-and-log-retention-settings-organization",
            put(settings),
        ),
        (
            "actions/get-artifact-and-log-retention-settings-repository",
            get(settings),
        ),
        (
            "actions/set-artifact-and-log-retention-settings-repository",
            put(settings),
        ),
        (
            "actions/get-fork-pr-contributor-approval-permissions-organization",
            get(settings),
        ),
        (
            "actions/set-fork-pr-contributor-approval-permissions-organization",
            put(settings),
        ),
        (
            "actions/get-fork-pr-contributor-approval-permissions-repository",
            get(settings),
        ),
        (
            "actions/set-fork-pr-contributor-approval-permissions-repository",
            put(settings),
        ),
        (
            "actions/get-github-actions-default-workflow-permissions-organization",
            get(settings),
        ),
        (
            "actions/set-github-actions-default-workflow-permissions-organization",
            put(settings),
        ),
        (
            "actions/get-github-actions-default-workflow-permissions-repository",
            get(settings),
        ),
        (
            "actions/set-github-actions-default-workflow-permissions-repository",
            put(settings),
        ),
        (
            "actions/get-private-repo-fork-pr-workflows-settings-organization",
            get(settings),
        ),
        (
            "actions/set-private-repo-fork-pr-workflows-settings-organization",
            put(settings),
        ),
        (
            "actions/get-private-repo-fork-pr-workflows-settings-repository",
            get(settings),
        ),
        (
            "actions/set-private-repo-fork-pr-workflows-settings-repository",
            put(settings),
        ),
        ("actions/get-workflow-access-to-repository", get(settings)),
        ("actions/set-workflow-access-to-repository", put(settings)),
        (
            "actions/get-github-actions-permissions-organization",
            get(organization_switch),
        ),
        (
            "actions/set-github-actions-permissions-organization",
            put(organization_switch),
        ),
        (
            "actions/get-self-hosted-runners-permissions-organization",
            get(organization_switch),
        ),
        (
            "actions/set-self-hosted-runners-permissions-organization",
            put(organization_switch),
        ),
        (
            "actions/list-selected-repositories-enabled-github-actions-organization",
            get(organization_repositories),
        ),
        (
            "actions/set-selected-repositories-enabled-github-actions-organization",
            put(organization_repositories),
        ),
        (
            "actions/enable-selected-repository-github-actions-organization",
            put(organization_repositories),
        ),
        (
            "actions/disable-selected-repository-github-actions-organization",
            delete(organization_repositories),
        ),
        (
            "actions/list-selected-repositories-self-hosted-runners-organization",
            get(organization_repositories),
        ),
        (
            "actions/set-selected-repositories-self-hosted-runners-organization",
            put(organization_repositories),
        ),
        (
            "actions/enable-selected-repository-self-hosted-runners-organization",
            put(organization_repositories),
        ),
        (
            "actions/disable-selected-repository-self-hosted-runners-organization",
            delete(organization_repositories),
        ),
        ("actions/get-org-actions-policies", get(policies)),
        ("actions/create-org-actions-policy", post(policies)),
        ("actions/get-org-actions-policy", get(policies)),
        ("actions/update-org-actions-policy", put(policies)),
        ("actions/delete-org-actions-policy", delete(policies)),
        ("actions/get-repo-actions-policies", get(policies)),
        ("actions/create-repo-actions-policy", post(policies)),
        ("actions/get-repo-actions-policy", get(policies)),
        ("actions/update-repo-actions-policy", put(policies)),
        ("actions/delete-repo-actions-policy", delete(policies)),
    ]
}
