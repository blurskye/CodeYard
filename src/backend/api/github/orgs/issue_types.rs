//! Organization issue types are the same definitions inherited by repository issues.
use crate::backend::api::github::orgs::auth;
use crate::backend::api::github::orgs::target;
use crate::backend::api::github::orgs::AppState;
use crate::backend::api::github::orgs::Error;
use crate::backend::api::github::orgs::Result;

use codeyard_db_repos::issue_attributes::Definition;

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
    let types = codeyard_db_repos::issue_attributes::organization_catalog(
        &state.pool,
        org.id,
        actor.user.id,
        "type",
    )
    .await
    .map_err(|error| crate::backend::api::github::issue_attributes::error(error).simple())?;
    Ok(Json(Value::Array(
        types
            .iter()
            .map(crate::backend::webhooks::payloads::issue_type)
            .collect(),
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
    let Json(input) =
        input.map_err(|error| crate::backend::api::github::json_error(error).simple())?;
    let id = params
        .get("issue_type_id")
        .map(|id| id.parse::<i64>().map_err(|_| Error::missing()))
        .transpose()?;
    let value = codeyard_db_repos::issue_attributes::configure_organization(
        &state.pool,
        org.id,
        actor.database_actor(),
        &Definition {
            id,
            expected_updated: None,
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
    .map_err(|error| crate::backend::api::github::issue_attributes::error(error).simple())?;
    Ok(Json(crate::backend::webhooks::payloads::issue_type(&value)))
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
    codeyard_db_repos::issue_attributes::delete_organization_definition(
        &state.pool,
        org.id,
        actor.database_actor(),
        id,
        "type",
    )
    .await
    .map_err(|error| crate::backend::api::github::issue_attributes::error(error).simple())?;
    Ok(StatusCode::NO_CONTENT)
}
