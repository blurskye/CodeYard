//! Organization security configurations share the repository security settings store.
use crate::pagination::Cursor;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use codeyard_types::urls::api_url;
use codeyard_webhooks::payloads;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

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
    match error.downcast_ref::<codeyard_database::queries::code_security::Refusal>() {
        Some(codeyard_database::queries::code_security::Refusal::Missing) => Error::missing(),
        Some(codeyard_database::queries::code_security::Refusal::Denied) => Error::new(
            StatusCode::FORBIDDEN,
            "Must have organization security access",
        ),
        Some(codeyard_database::queries::code_security::Refusal::Duplicate) => {
            Error::invalid_simple("A security configuration with this name already exists")
        }
        Some(codeyard_database::queries::code_security::Refusal::Invalid) => Error::invalid_simple(
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
) -> Result<(codeyard_identity::github_auth::Authentication, i64)> {
    let auth = codeyard_identity::github_auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    if write {
        auth.require_write()?;
    }
    let org = crate::orgs::target(state, org).await?;
    Ok((auth, org.id))
}
fn document(
    row: &codeyard_database::queries::code_security::Configuration,
    org: &str,
) -> Result<Value> {
    let mut value: Value = serde_json::from_str(&row.settings).map_err(Error::unavailable)?;
    value["id"] = json!(row.id);
    value["target_type"] = json!("organization");
    value["url"] = json!(api_url(&format!(
        "/orgs/{}/code-security/configurations/{}",
        codeyard_types::paths::encode_component(org),
        row.id
    )));
    value["html_url"] = json!(format!(
        "{}/settings/organizations/{}/security#code-security-{}",
        codeyard_types::http_config::link_origin(),
        row.org_id,
        row.id
    ));
    value["created_at"] = json!(row.created_at);
    value["updated_at"] = json!(row.updated_at);
    Ok(value)
}
/// One cursor page of `rows`, which were read with one extra row to tell whether more follow.
fn page<T>(
    cursor: &Cursor,
    uri: &Uri,
    mut rows: Vec<T>,
    id: impl Fn(&T) -> i64,
    document: impl Fn(&T) -> Result<Value>,
) -> Result<Response> {
    let more = cursor.trim(&mut rows)?;
    let (headers, _) = cursor.links(uri, rows.first().map(&id), rows.last().map(&id), more)?;
    let values = rows.iter().map(document).collect::<Result<Vec<_>>>()?;
    Ok((headers, Json(values)).into_response())
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Query(query): Query<Cursor>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (actor, id) = scope(&state, &headers, &org, false).await?;
    let (after, before, limit) = query.window()?;
    let rows = codeyard_db_security::code_security::list_window(
        &state.pool,
        actor.user.id,
        id,
        after,
        before,
        limit as i64 + 1,
    )
    .await
    .map_err(failure)?;
    page(&query, &uri, rows, |row| row.id, |row| document(row, &org))
}
async fn get_configuration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, i64)>,
) -> Result<Json<Value>> {
    let (actor, owner) = scope(&state, &headers, &org, false).await?;
    let row = codeyard_db_security::code_security::list(&state.pool, actor.user.id, owner)
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
    let row = codeyard_db_security::code_security::save(
        &state.pool,
        actor.database_actor(),
        owner,
        id,
        &input,
    )
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
    codeyard_db_security::code_security::delete(&state.pool, actor.database_actor(), owner, id)
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
    codeyard_db_security::code_security::attach(
        &state.pool,
        actor.database_actor(),
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
    codeyard_db_security::code_security::attach(
        &state.pool,
        actor.database_actor(),
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
    let row = codeyard_db_security::code_security::set_default(
        &state.pool,
        actor.database_actor(),
        owner,
        id,
        &input.default_for_new_repos,
    )
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
    let rows = codeyard_db_security::code_security::list(&state.pool, actor.user.id, owner)
        .await
        .map_err(failure)?;
    let values=rows.iter().filter(|row|row.default_for_new_repos!="none").map(|row|Ok(json!({"default_for_new_repos":row.default_for_new_repos,"configuration":document(row,&org)?}))).collect::<Result<Vec<_>>>()?;
    Ok(Json(json!(values)))
}
async fn repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, i64)>,
    Query(query): Query<Cursor>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (actor, owner) = scope(&state, &headers, &org, false).await?;
    let (after, before, limit) = query.window()?;
    let rows = codeyard_db_security::code_security::repositories(
        &state.pool,
        actor.user.id,
        owner,
        id,
        after,
        before,
        limit as i64 + 1,
    )
    .await
    .map_err(failure)?;
    page(
        &query,
        &uri,
        rows,
        |repo| repo.id,
        |repo| Ok(json!({"status":"attached","repository":payloads::repository(repo,&org)})),
    )
}
async fn repository_configuration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    if !codeyard_identity::repository_access::can_admin(&state.pool, &auth.user, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Repository administration required",
        ));
    }
    let row = codeyard_db_security::code_security::repository_configuration(&state.pool, repo.id)
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
