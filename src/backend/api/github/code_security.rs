//! Organization security configurations share the repository security settings store.
use super::{api_url, auth, orgs, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    app::AppState, db::code_security as records, repositories::access, webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("code-security/get-configurations-for-org", get(list)),
        ("code-security/create-configuration", post(save)),
        ("code-security/get-default-configurations", get(defaults)),
        ("code-security/get-configuration", get(get_configuration)),
        ("code-security/update-configuration", patch(save)),
        ("code-security/delete-configuration", delete(remove)),
        ("code-security/attach-configuration", post(attach)),
        ("code-security/detach-configuration", delete(detach)),
        (
            "code-security/set-configuration-as-default",
            put(set_default),
        ),
        (
            "code-security/get-repositories-for-configuration",
            get(repositories),
        ),
        (
            "code-security/get-configuration-for-repository",
            get(repository_configuration),
        ),
    ]
}
fn failure(error: anyhow::Error) -> Error {
    match error.downcast_ref::<records::Refusal>() {
        Some(records::Refusal::Missing) => Error::missing(),
        Some(records::Refusal::Denied) => Error::new(
            StatusCode::FORBIDDEN,
            "Must have organization security access",
        ),
        Some(records::Refusal::Duplicate) => {
            Error::invalid_simple("A security configuration with this name already exists")
        }
        Some(records::Refusal::Invalid) => Error::invalid_simple(
            "Invalid security configuration; CodeQL default setup cannot be enabled on Codeyard",
        ),
        None => Error::unavailable(error),
    }
}
async fn scope(
    state: &AppState,
    headers: &HeaderMap,
    org: &str,
    write: bool,
) -> Result<(i64, i64)> {
    let auth = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    if write {
        auth.require_write()?;
    }
    let org = orgs::target(state, org).await?;
    Ok((auth.user.id, org.id))
}
fn document(row: &records::Configuration, org: &str) -> Result<Value> {
    let mut value: Value = serde_json::from_str(&row.settings).map_err(Error::unavailable)?;
    value["id"] = json!(row.id);
    value["target_type"] = json!("organization");
    value["url"] = json!(api_url(&format!(
        "/orgs/{}/code-security/configurations/{}",
        crate::webui::encode_component(org),
        row.id
    )));
    value["html_url"] = json!(format!(
        "{}/settings/organizations/{}/security#code-security-{}",
        crate::backend::api::http::config::link_origin(),
        row.org_id,
        row.id
    ));
    value["created_at"] = json!(row.created_at);
    value["updated_at"] = json!(row.updated_at);
    Ok(value)
}
fn page(page: &Pagination, uri: &Uri, values: Vec<Value>) -> Result<Response> {
    let (offset, limit) = page.window()?;
    let values: Vec<_> = values.into_iter().skip(offset).take(limit + 1).collect();
    let next = values.len() > limit;
    page.response(uri, values, next)
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Query(query): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (actor, id) = scope(&state, &headers, &org, false).await?;
    let rows = records::list(&state.pool, actor, id)
        .await
        .map_err(failure)?;
    page(
        &query,
        &uri,
        rows.iter()
            .map(|row| document(row, &org))
            .collect::<Result<Vec<_>>>()?,
    )
}
async fn get_configuration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, i64)>,
) -> Result<Json<Value>> {
    let (actor, owner) = scope(&state, &headers, &org, false).await?;
    let row = records::list(&state.pool, actor, owner)
        .await
        .map_err(failure)?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or_else(Error::missing)?;
    Ok(Json(document(&row, &org)?))
}
async fn save(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    input: std::result::Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Response> {
    let org = &params["org"];
    let (actor, owner) = scope(&state, &headers, org, true).await?;
    let id = params
        .get("configuration_id")
        .map(|id| id.parse::<i64>().map_err(|_| Error::missing()))
        .transpose()?;
    let Json(input) =
        input.map_err(|_| Error::invalid_simple("Invalid security configuration request"))?;
    let row = records::save(&state.pool, actor, owner, id, &input)
        .await
        .map_err(failure)?;
    Ok((
        if id.is_some() {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        Json(document(&row, org)?),
    )
        .into_response())
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, i64)>,
) -> Result<StatusCode> {
    let (actor, owner) = scope(&state, &headers, &org, true).await?;
    records::delete(&state.pool, actor, owner, id)
        .await
        .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attach {
    scope: String,
    #[serde(default)]
    selected_repository_ids: Vec<i64>,
}
async fn attach(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, i64)>,
    input: std::result::Result<Json<Attach>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode> {
    let (actor, owner) = scope(&state, &headers, &org, true).await?;
    let Json(input) =
        input.map_err(|_| Error::invalid_simple("Invalid security configuration attachment"))?;
    records::attach(
        &state.pool,
        actor,
        owner,
        Some(id),
        &input.scope,
        &input.selected_repository_ids,
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::ACCEPTED)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selected {
    selected_repository_ids: Vec<i64>,
}
async fn detach(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Json(input): Json<Selected>,
) -> Result<StatusCode> {
    let (actor, owner) = scope(&state, &headers, &org, true).await?;
    records::attach(
        &state.pool,
        actor,
        owner,
        None,
        "selected",
        &input.selected_repository_ids,
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultScope {
    default_for_new_repos: String,
}
async fn set_default(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, i64)>,
    input: std::result::Result<Json<DefaultScope>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>> {
    let (actor, owner) = scope(&state, &headers, &org, true).await?;
    let Json(input) =
        input.map_err(|_| Error::invalid_simple("Invalid security configuration default"))?;
    let row = records::set_default(&state.pool, actor, owner, id, &input.default_for_new_repos)
        .await
        .map_err(failure)?;
    Ok(Json(
        json!({"default_for_new_repos":row.default_for_new_repos,"configuration":document(&row,&org)?}),
    ))
}
async fn defaults(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<Value>> {
    let (actor, owner) = scope(&state, &headers, &org, false).await?;
    let rows = records::list(&state.pool, actor, owner)
        .await
        .map_err(failure)?;
    let values=rows.iter().filter(|row|row.default_for_new_repos!="none").map(|row|Ok(json!({"default_for_new_repos":row.default_for_new_repos,"configuration":document(row,&org)?}))).collect::<Result<Vec<_>>>()?;
    Ok(Json(json!(values)))
}
async fn repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, i64)>,
    Query(query): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (actor, owner) = scope(&state, &headers, &org, false).await?;
    let rows = records::repositories(&state.pool, actor, owner, id)
        .await
        .map_err(failure)?;
    page(
        &query,
        &uri,
        rows.iter()
            .map(|repo| json!({"status":"attached","repository":payloads::repository(repo,&org)}))
            .collect(),
    )
}
async fn repository_configuration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let auth = auth.ok_or_else(auth::required)?;
    if !access::can_admin(&state.pool, &auth.user, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Repository administration required",
        ));
    }
    let row = records::repository_configuration(&state.pool, repo.id)
        .await
        .map_err(failure)?;
    Ok(match row {
        Some(row) => Json(json!({"status":"attached","configuration":document(&row,&owner)?}))
            .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

#[cfg(test)]
mod tests;
