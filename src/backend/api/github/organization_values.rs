//! Organization Actions values share visibility checks with workflow delivery.
use super::{auth, pagination::Pagination, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        organization_values::{
            self as values, Kind, OrganizationValue, Save, Selection, ValueError,
        },
    },
};
use axum::{
    extract::{rejection::JsonRejection, MatchedPath, OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put, MethodRouter},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/list-org-secrets", get(list)),
        ("actions/get-org-public-key", get(public_key)),
        ("actions/get-org-secret", get(value)),
        ("actions/create-or-update-org-secret", put(save_secret)),
        ("actions/delete-org-secret", delete(remove)),
        (
            "actions/list-selected-repos-for-org-secret",
            get(repositories),
        ),
        (
            "actions/set-selected-repos-for-org-secret",
            put(set_repositories),
        ),
        (
            "actions/add-selected-repo-to-org-secret",
            put(add_repository),
        ),
        (
            "actions/remove-selected-repo-from-org-secret",
            delete(remove_repository),
        ),
        ("actions/list-org-variables", get(list)),
        ("actions/create-org-variable", post(create_variable)),
        ("actions/get-org-variable", get(value)),
        ("actions/update-org-variable", patch(update_variable)),
        ("actions/delete-org-variable", delete(remove)),
        (
            "actions/list-selected-repos-for-org-variable",
            get(repositories),
        ),
        (
            "actions/set-selected-repos-for-org-variable",
            put(set_repositories),
        ),
        (
            "actions/add-selected-repo-to-org-variable",
            put(add_repository),
        ),
        (
            "actions/remove-selected-repo-from-org-variable",
            delete(remove_repository),
        ),
        ("actions/list-repo-organization-secrets", get(inherited)),
        ("actions/list-repo-organization-variables", get(inherited)),
    ]
}

pub(super) fn error(error: ValueError) -> Error {
    match error {
        ValueError::Denied => Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token"),
        ValueError::NotFound => Error::missing(),
        ValueError::NotSelected => Error::new(StatusCode::CONFLICT, error.to_string()),
        ValueError::Exists => Error::validation("ActionsValue", "name", "already_exists"),
        ValueError::InvalidName => Error::validation("ActionsValue", "name", "invalid"),
        ValueError::Repository => {
            Error::validation("ActionsValue", "selected_repository_ids", "invalid")
        }
        ValueError::InvalidValue | ValueError::Visibility | ValueError::Limit | ValueError::SecretsSize => {
            Error::invalid(error.to_string())
        }
        _ => Error::unavailable(error),
    }
}

fn kind(path: &MatchedPath) -> Kind {
    if path.as_str().contains("/actions/variables")
        || path.as_str().contains("/actions/organization-variables")
    {
        Kind::Variable
    } else {
        Kind::Secret
    }
}
fn plural(kind: Kind) -> &'static str {
    if kind == Kind::Secret {
        "secrets"
    } else {
        "variables"
    }
}
fn name(params: &HashMap<String, String>) -> Result<String> {
    params
        .get("secret_name")
        .or_else(|| params.get("name"))
        .map(|name| name.to_ascii_uppercase())
        .ok_or_else(Error::missing)
}
async fn organization(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    write: bool,
) -> Result<(i64, auth::Authentication)> {
    let auth = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    if write {
        auth.require_write()?;
    }
    let org = db::organizations::get_public_organization(&state.pool, name)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    match db::organizations::get_organization_role(&state.pool, auth.user.id, org.id)
        .await
        .map_err(Error::unavailable)?
    {
        db::organizations::OrganizationOutcome::Applied(role)
            if db::organizations::administers(role) => {}
        _ => {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "Resource not accessible by token",
            ))
        }
    }
    Ok((org.id, auth))
}
fn document(org: &str, kind: Kind, row: &OrganizationValue) -> Value {
    let mut value = json!({"name":row.name,"created_at":row.created_at,"updated_at":row.updated_at,"visibility":row.visibility,"selected_repositories_url":super::api_url(&format!("/orgs/{org}/actions/{}/{}/repositories", plural(kind), crate::webui::encode_component(&row.name)))});
    if kind == Kind::Variable {
        value["value"] = json!(row.value);
    }
    value
}
fn page(page: &Pagination, uri: &Uri, key: &str, rows: Vec<Value>) -> Result<Response> {
    let total = rows.len();
    let (offset, limit) = page.window()?;
    let items: Vec<_> = rows.into_iter().skip(offset).take(limit).collect();
    let mut body = json!({"total_count":total});
    body[key] = json!(items);
    Ok((
        page.headers(uri, offset.saturating_add(limit) < total)?,
        Json(body),
    )
        .into_response())
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
    route: MatchedPath,
    OriginalUri(uri): OriginalUri,
    Query(pagination): Query<Pagination>,
) -> Result<Response> {
    let (id, auth) = organization(&state, &headers, &org, false).await?;
    let kind = kind(&route);
    let rows = values::list(&state.pool, auth.user.id, id, kind)
        .await
        .map_err(error)?;
    page(
        &pagination,
        &uri,
        plural(kind),
        rows.iter()
            .map(|row| document(&crate::webui::encode_component(&org), kind, row))
            .collect(),
    )
}
async fn public_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<Value>> {
    let (org, auth) = organization(&state, &headers, &org, false).await?;
    let encryptor = state
        .secret_encryptor
        .as_deref()
        .ok_or_else(|| Error::unavailable(()))?;
    let (key_id, key) = values::key(&state.pool, encryptor, auth.user.id, org)
        .await
        .map_err(error)?;
    Ok(Json(
        json!({"key_id":key_id,"key":STANDARD.encode(key.public_key().as_bytes())}),
    ))
}
async fn value(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    route: MatchedPath,
) -> Result<Json<Value>> {
    let (org, auth) = organization(&state, &headers, &params["org"], false).await?;
    let kind = kind(&route);
    let name = name(&params)?;
    let row = values::list(&state.pool, auth.user.id, org, kind)
        .await
        .map_err(error)?
        .into_iter()
        .find(|row| row.name == name)
        .ok_or_else(Error::missing)?;
    Ok(Json(document(
        &crate::webui::encode_component(&params["org"]),
        kind,
        &row,
    )))
}

#[derive(Deserialize)]
struct SecretInput {
    encrypted_value: String,
    key_id: String,
    visibility: String,
    selected_repository_ids: Option<Vec<i64>>,
}
async fn save_secret(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    body: std::result::Result<Json<SecretInput>, JsonRejection>,
) -> Result<Response> {
    let (org, auth) = organization(&state, &headers, &params["org"], true).await?;
    let Json(body) = body.map_err(super::json_error)?;
    let encryptor = state
        .secret_encryptor
        .as_deref()
        .ok_or_else(|| Error::unavailable(()))?;
    let (id, key) = values::key(&state.pool, encryptor, auth.user.id, org)
        .await
        .map_err(error)?;
    let value = super::action_secrets::open_value(&key, &id, &body.key_id, &body.encrypted_value)?;
    let created = values::save(
        &state.pool,
        Some(encryptor),
        auth.user.id,
        org,
        &Save {
            kind: Kind::Secret,
            previous: None,
            name: &name(&params)?,
            value: Some(&value),
            visibility: Some(&body.visibility),
            repositories: body.selected_repository_ids.as_deref(),
        },
    )
    .await
    .map_err(error)?;
    Ok(if created {
        (StatusCode::CREATED, Json(json!({}))).into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    })
}
#[derive(Deserialize)]
struct CreateVariable {
    name: String,
    #[serde(default)]
    value: String,
    visibility: String,
    selected_repository_ids: Option<Vec<i64>>,
}
async fn create_variable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
    body: std::result::Result<Json<CreateVariable>, JsonRejection>,
) -> Result<Response> {
    let (org, auth) = organization(&state, &headers, &org, true).await?;
    let Json(body) = body.map_err(super::json_error)?;
    values::save(
        &state.pool,
        None,
        auth.user.id,
        org,
        &Save {
            kind: Kind::Variable,
            previous: None,
            name: &body.name.to_ascii_uppercase(),
            value: Some(&body.value),
            visibility: Some(&body.visibility),
            repositories: body.selected_repository_ids.as_deref(),
        },
    )
    .await
    .map_err(|failure| match failure {
        ValueError::Exists => Error::new(StatusCode::CONFLICT, "Variable already exists"),
        other => error(other),
    })?;
    Ok((StatusCode::CREATED, Json(json!({}))).into_response())
}
#[derive(Deserialize)]
struct UpdateVariable {
    name: Option<String>,
    value: Option<String>,
    visibility: Option<String>,
    selected_repository_ids: Option<Vec<i64>>,
}
async fn update_variable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org, previous)): Path<(String, String)>,
    body: std::result::Result<Json<UpdateVariable>, JsonRejection>,
) -> Result<StatusCode> {
    let (org, auth) = organization(&state, &headers, &org, true).await?;
    let Json(body) = body.map_err(super::json_error)?;
    let previous = previous.to_ascii_uppercase();
    let value = body
        .value
        .as_deref()
        .or_else(|| body.name.is_none().then_some(""));
    let name = body
        .name
        .as_deref()
        .unwrap_or(&previous)
        .to_ascii_uppercase();
    values::save(
        &state.pool,
        None,
        auth.user.id,
        org,
        &Save {
            kind: Kind::Variable,
            previous: Some(&previous),
            name: &name,
            value,
            visibility: body.visibility.as_deref(),
            repositories: body.selected_repository_ids.as_deref(),
        },
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    route: MatchedPath,
) -> Result<StatusCode> {
    let (org, auth) = organization(&state, &headers, &params["org"], true).await?;
    values::delete(
        &state.pool,
        auth.user.id,
        org,
        kind(&route),
        &name(&params)?,
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    route: MatchedPath,
    OriginalUri(uri): OriginalUri,
    Query(pagination): Query<Pagination>,
) -> Result<Response> {
    let (org, auth) = organization(&state, &headers, &params["org"], false).await?;
    let rows = values::repositories(
        &state.pool,
        auth.user.id,
        org,
        kind(&route),
        &name(&params)?,
    )
    .await
    .map_err(error)?;
    let total = rows.len();
    let (offset, limit) = pagination.window()?;
    let mut items = Vec::new();
    for row in rows.into_iter().skip(offset).take(limit) {
        items.push(super::repos::document(&state, &row, Some(&auth)).await?);
    }
    Ok((
        pagination.headers(&uri, offset.saturating_add(limit) < total)?,
        Json(json!({"total_count":total,"repositories":items})),
    )
        .into_response())
}
#[derive(Deserialize)]
struct Repositories {
    selected_repository_ids: Vec<i64>,
}
async fn set_repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    route: MatchedPath,
    body: std::result::Result<Json<Repositories>, JsonRejection>,
) -> Result<StatusCode> {
    let (org, auth) = organization(&state, &headers, &params["org"], true).await?;
    let Json(body) = body.map_err(super::json_error)?;
    values::select(
        &state.pool,
        auth.user.id,
        org,
        kind(&route),
        &name(&params)?,
        Selection::Set(&body.selected_repository_ids),
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn select_repository(
    state: &AppState,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
    kind: Kind,
    add: bool,
) -> Result<StatusCode> {
    let (org, auth) = organization(state, headers, &params["org"], true).await?;
    let id = params["repository_id"]
        .parse()
        .map_err(|_| Error::missing())?;
    values::select(
        &state.pool,
        auth.user.id,
        org,
        kind,
        &name(params)?,
        if add {
            Selection::Add(id)
        } else {
            Selection::Remove(id)
        },
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn add_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    route: MatchedPath,
) -> Result<StatusCode> {
    select_repository(&state, &headers, &params, kind(&route), true).await
}
async fn remove_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    route: MatchedPath,
) -> Result<StatusCode> {
    select_repository(&state, &headers, &params, kind(&route), false).await
}
async fn inherited(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    route: MatchedPath,
    OriginalUri(uri): OriginalUri,
    Query(pagination): Query<Pagination>,
) -> Result<Response> {
    let (repo, auth) = super::repository(&state, &headers, &owner, &name, false).await?;
    let auth = auth.ok_or_else(auth::required)?;
    let kind = kind(&route);
    let rows = values::inherited(&state.pool, auth.user.id, repo.id, kind)
        .await
        .map_err(error)?;
    page(&pagination, &uri, plural(kind), rows.into_iter().map(|row| {
        let mut value = json!({"name":row.name,"created_at":row.created_at,"updated_at":row.updated_at});
        if kind==Kind::Variable { value["value"]=json!(row.value); }
        value
    }).collect())
}
