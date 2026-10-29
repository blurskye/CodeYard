//! Organization issue types are the same definitions inherited by repository issues.
use super::{auth, target, AppState, Error, Result};
use crate::backend::{
    db::issue_attributes::{self, Definition},
    webhooks::payloads,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

type Params = HashMap<String, String>;
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/list-issue-types", get(list)),
        ("orgs/create-issue-type", post(save)),
        ("orgs/update-issue-type", put(save)),
        ("orgs/delete-issue-type", delete(remove)),
    ]
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
    let types = issue_attributes::organization_catalog(&state.pool, org.id, actor.user.id, "type")
        .await
        .map_err(|error| super::super::issue_attributes::error(error).simple())?;
    Ok(Json(Value::Array(
        types.iter().map(payloads::issue_type).collect(),
    )))
}
#[derive(Deserialize)]
struct Input {
    name: String,
    is_enabled: bool,
    description: Option<String>,
    color: Option<String>,
}
async fn save(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    input: std::result::Result<Json<Input>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(&state, &params["org"]).await?;
    let Json(input) = input.map_err(|_| Error::invalid_simple("Invalid issue type"))?;
    let id = params
        .get("issue_type_id")
        .map(|id| id.parse::<i64>().map_err(|_| Error::missing()))
        .transpose()?;
    let value = issue_attributes::configure_organization(
        &state.pool,
        org.id,
        actor.user.id,
        &Definition {
            id,
            kind: "type".into(),
            name: input.name,
            description: input.description,
            data_type: None,
            visibility: "all".into(),
            is_enabled: input.is_enabled,
            color: input.color,
            options: Vec::new(),
        },
    )
    .await
    .map_err(|error| super::super::issue_attributes::error(error).simple())?;
    Ok(Json(payloads::issue_type(&value)))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
) -> Result<StatusCode> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(&state, &name).await?;
    issue_attributes::delete_organization_definition(
        &state.pool,
        org.id,
        actor.user.id,
        id,
        "type",
    )
    .await
    .map_err(|error| super::super::issue_attributes::error(error).simple())?;
    Ok(StatusCode::NO_CONTENT)
}
