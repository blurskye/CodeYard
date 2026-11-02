//! Organization repository property definitions and values share native transactions.
use super::super::pagination::Pagination;
use super::{auth, outcome, target, AppState, Error, Result};
use crate::backend::{
    db::organizations::properties::{
        self, query::RepositoryQuery, DefinitionInput, Refusal, ValueInput,
    },
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{delete, get, patch, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "orgs/custom-properties-for-repos-get-organization-definitions",
            get(list),
        ),
        (
            "orgs/custom-properties-for-repos-create-or-update-organization-definitions",
            patch(define_many),
        ),
        (
            "orgs/custom-properties-for-repos-get-organization-definition",
            get(get_definition),
        ),
        (
            "orgs/custom-properties-for-repos-create-or-update-organization-definition",
            put(define_one),
        ),
        (
            "orgs/custom-properties-for-repos-delete-organization-definition",
            delete(remove),
        ),
        (
            "orgs/custom-properties-for-repos-get-organization-values",
            get(values),
        ),
        (
            "orgs/custom-properties-for-repos-create-or-update-organization-values",
            patch(set_values),
        ),
    ]
}
pub(crate) fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Invalid { field, message }) => Error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Validation Failed".into(),
            Some(
                json!([{"resource":"CustomProperty","field":field,"code":"invalid","message":message}]),
            ),
        ),
        Some(Refusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Repository property access denied")
        }
        Some(Refusal::Missing) => Error::missing(),
        Some(Refusal::TooLarge) => Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request a smaller repository property page",
        ),
        None => Error::unavailable(error),
    }
}
fn definition(mut value: Value, name: Option<&str>) -> Result<DefinitionInput> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| Error::validation("CustomProperty", "properties", "invalid"))?;
    if let Some(source) = object.get("source_type") {
        if source != "organization" {
            return Err(Error::validation(
                "CustomProperty",
                "source_type",
                "invalid",
            ));
        }
    }
    if let Some(url) = object.get("url") {
        if !url.as_str().is_some_and(|url| url::Url::parse(url).is_ok()) {
            return Err(Error::validation("CustomProperty", "url", "invalid"));
        }
    }
    if let Some(name) = name {
        object.insert("property_name".into(), json!(name));
    }
    serde_json::from_value(value)
        .map_err(|_| Error::invalid("Invalid repository property definition"))
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let definitions = outcome(
        properties::catalog(&state.pool, actor.user.id, org.id)
            .await
            .map_err(error)?,
    )?;
    Ok(Json(Value::Array(
        definitions
            .iter()
            .map(|definition| payloads::repository_property(definition, &org.name))
            .collect(),
    )))
}
async fn get_definition(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, property)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let definitions = outcome(
        properties::catalog(&state.pool, actor.user.id, org.id)
            .await
            .map_err(error)?,
    )?;
    let definition = definitions
        .iter()
        .find(|definition| {
            definition
                .fields
                .property_name
                .eq_ignore_ascii_case(&property)
        })
        .ok_or_else(Error::missing)?;
    Ok(Json(payloads::repository_property(definition, &org.name)))
}
async fn define_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, property)): Path<(String, String)>,
    input: std::result::Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(&state, &name).await?;
    let Json(input) =
        input.map_err(|_| Error::invalid("Invalid repository property definition"))?;
    let input = definition(input, Some(&property))?;
    let definitions = outcome(
        properties::define(&state.pool, actor.user.id, org.id, &[input])
            .await
            .map_err(error)?,
    )?;
    let definition = definitions.first().ok_or_else(|| Error::unavailable(()))?;
    Ok(Json(payloads::repository_property(definition, &org.name)))
}
#[derive(Deserialize)]
struct Definitions {
    properties: Vec<Value>,
}
async fn define_many(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    input: std::result::Result<Json<Definitions>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(&state, &name).await?;
    let Json(input) =
        input.map_err(|_| Error::invalid("Invalid repository property definitions"))?;
    let inputs = input
        .properties
        .into_iter()
        .map(|value| definition(value, None))
        .collect::<Result<Vec<_>>>()?;
    let definitions = outcome(
        properties::define(&state.pool, actor.user.id, org.id, &inputs)
            .await
            .map_err(error)?,
    )?;
    Ok(Json(Value::Array(
        definitions
            .iter()
            .map(|definition| payloads::repository_property(definition, &org.name))
            .collect(),
    )))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, property)): Path<(String, String)>,
) -> Result<StatusCode> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(&state, &name).await?;
    outcome(
        properties::remove(&state.pool, actor.user.id, org.id, &property)
            .await
            .map_err(error)?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}
async fn values(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let query = RepositoryQuery::parse(query.get("repository_query").map_or("", String::as_str))
        .map_err(error)?;
    let (offset, limit) = page.window()?;
    let mut rows = outcome(
        properties::list_values(
            &state.pool,
            actor.user.id,
            org.id,
            &query,
            (offset as i64, (limit + 1) as i64),
        )
        .await
        .map_err(error)?,
    )?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    page.response(
        &uri,
        rows.iter()
            .map(|row| payloads::repository_property_values(row, &org.name))
            .collect::<Vec<_>>(),
        more,
    )
}
#[derive(Deserialize)]
struct Values {
    repository_names: Vec<String>,
    properties: Vec<ValueInput>,
}
async fn set_values(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    input: std::result::Result<Json<Values>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(&state, &name).await?;
    let Json(input) = input.map_err(|_| Error::invalid("Invalid repository property values"))?;
    outcome(
        properties::set_values(
            &state.pool,
            actor.user.id,
            org.id,
            &input.repository_names,
            &input.properties,
        )
        .await
        .map_err(error)?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}
