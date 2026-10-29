//! External property values remain separate from organization-defined custom properties.
use super::{auth, outcome, target, AppState, Error, Result};
use crate::backend::db::organizations::external_properties::{self as data, RepositoryValue};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, patch, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "orgs/external-properties-for-repos-get-organization-app-installations",
            get(installations),
        ),
        (
            "orgs/external-properties-for-repos-register-organization-app-installation",
            post(register),
        ),
        (
            "orgs/external-properties-for-repos-get-organization-definitions",
            get(definitions),
        ),
        (
            "orgs/external-properties-for-repos-create-or-update-organization-values",
            patch(set_values),
        ),
        (
            "orgs/external-properties-for-repos-create-or-update-values-for-organization-property",
            patch(set_property),
        ),
        (
            "orgs/external-properties-for-repos-delete-organization-values",
            delete(delete_values),
        ),
    ]
}

async fn scope(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    write: bool,
) -> Result<(i64, i64)> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    if write {
        actor.require_write()?;
    }
    let org = target(state, name).await?;
    Ok((actor.user.id, org.id))
}
fn installation(row: data::Installation) -> Value {
    json!({"display_name": row.display_name, "installation": {"id": row.installation_id}})
}
async fn installations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let (actor, org) = scope(&state, &headers, &name, false).await?;
    let rows = outcome(
        data::installations(&state.pool, actor, org)
            .await
            .map_err(Error::unavailable)?,
    )?;
    Ok(Json(rows.into_iter().map(installation).collect()))
}
#[derive(Deserialize)]
struct Registration {
    display_name: String,
    installation_id: Option<i64>,
}
async fn register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(input): Json<Registration>,
) -> Result<(StatusCode, Json<Value>)> {
    let (actor, org) = scope(&state, &headers, &name, true).await?;
    let row = outcome(
        data::register(
            &state.pool,
            actor,
            org,
            &input.display_name,
            input.installation_id,
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    Ok((StatusCode::CREATED, Json(installation(row))))
}
async fn definitions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let (actor, org) = scope(&state, &headers, &name, false).await?;
    let rows = outcome(
        data::definitions(&state.pool, actor, org)
            .await
            .map_err(Error::unavailable)?,
    )?;
    Ok(Json(
        rows.into_iter()
            .map(|name| json!({"property_name":name}))
            .collect(),
    ))
}
#[derive(Deserialize)]
struct Property {
    property_name: String,
    value: Value,
}
#[derive(Deserialize)]
struct Values {
    repository_names: Vec<String>,
    properties: Vec<Property>,
}
#[derive(Deserialize)]
struct PropertyValue {
    repository_name: String,
    value: Option<String>,
}
#[derive(Deserialize)]
struct PropertyValues {
    repository_values: Vec<PropertyValue>,
}
async fn set_values(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(input): Json<Values>,
) -> Result<StatusCode> {
    let (actor, org) = scope(&state, &headers, &name, true).await?;
    if input.repository_names.is_empty()
        || input.repository_names.len() > 30
        || input.properties.len() > 100
    {
        return Err(Error::validation(
            "ExternalProperty",
            "repository_names",
            "invalid",
        ));
    }
    let mut values = Vec::new();
    for repository in input.repository_names {
        for property in &input.properties {
            values.push(RepositoryValue {
                repository_name: repository.clone(),
                property_name: property.property_name.clone(),
                value: property.value.clone(),
            });
        }
    }
    outcome(
        data::set_values(&state.pool, actor, org, &values)
            .await
            .map_err(Error::unavailable)?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}
async fn set_property(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, property)): Path<(String, String)>,
    Json(input): Json<PropertyValues>,
) -> Result<StatusCode> {
    let (actor, org) = scope(&state, &headers, &name, true).await?;
    if input.repository_values.is_empty() || input.repository_values.len() > 100 {
        return Err(Error::validation(
            "ExternalProperty",
            "repository_values",
            "invalid",
        ));
    }
    let values = input
        .repository_values
        .into_iter()
        .map(|row| RepositoryValue {
            repository_name: row.repository_name,
            property_name: property.clone(),
            value: json!(row.value),
        })
        .collect::<Vec<_>>();
    outcome(
        data::set_values(&state.pool, actor, org, &values)
            .await
            .map_err(Error::unavailable)?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}
async fn delete_values(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, property)): Path<(String, String)>,
) -> Result<StatusCode> {
    let (actor, org) = scope(&state, &headers, &name, true).await?;
    outcome(
        data::delete_values(&state.pool, actor, org, &property)
            .await
            .map_err(Error::unavailable)?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}
