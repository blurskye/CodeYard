//! Organization issue types are the same definitions inherited by repository issues.
use crate::auth;
use crate::orgs::target;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_db_repos::issue_attributes::Definition;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

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
    let actor = codeyard_app::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let org = target(&state, &name).await?;
    let types = codeyard_db_repos::issue_attributes::organization_catalog(
        &state.pool,
        org.id,
        actor.user.id,
        "type",
    )
    .await
    .map_err(|error| crate::issues::issue_attributes::error(error).simple())?;
    Ok(Json(Value::Array(
        types
            .iter()
            .map(codeyard_app::webhooks::payloads::issue_type)
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
    let actor = codeyard_app::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    actor.require_write()?;
    let org = target(&state, &params["org"]).await?;
    let Json(input) =
        input.map_err(|error| codeyard_model::github_error::json_error(error).simple())?;
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
    .map_err(|error| crate::issues::issue_attributes::error(error).simple())?;
    Ok(Json(codeyard_app::webhooks::payloads::issue_type(&value)))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
) -> Result<StatusCode> {
    let actor = codeyard_app::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
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
    .map_err(|error| crate::issues::issue_attributes::error(error).simple())?;
    Ok(StatusCode::NO_CONTENT)
}
