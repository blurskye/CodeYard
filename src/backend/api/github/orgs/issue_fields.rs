//! Organization fields share inherited native definitions, typed values and stable select IDs.
use crate::backend::api::github::orgs::auth;
use crate::backend::api::github::orgs::target;
use crate::backend::api::github::orgs::AppState;
use crate::backend::api::github::orgs::Error;
use crate::backend::api::github::orgs::Result;

use codeyard_db_repos::issue_attributes::Definition;
use codeyard_db_repos::issue_attributes::FieldChanges;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, patch, post, MethodRouter},
    Json,
};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
type Params = HashMap<String, String>;

fn field_error(error: anyhow::Error) -> Error {
    let error = crate::backend::api::github::issue_attributes::error(error);
    // Organization field errors use strings; issue field-value errors use structured objects.
    if error.0 == StatusCode::UNPROCESSABLE_ENTITY {
        Error::invalid_simple(error.1)
    } else {
        error
    }
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/list-issue-fields", get(list)),
        ("orgs/create-issue-field", post(create)),
        ("orgs/update-issue-field", patch(update)),
        ("orgs/delete-issue-field", delete(remove)),
    ]
}
fn supplied<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
#[derive(Deserialize)]
struct SelectOption {
    id: Option<i64>,
    name: String,
    description: Option<String>,
    color: String,
    priority: i64,
}
impl From<SelectOption> for codeyard_db_repos::issue_attributes::OptionInput {
    fn from(option: SelectOption) -> Self {
        Self {
            id: option.id,
            name: option.name,
            description: option.description,
            color: option.color,
            priority: Some(option.priority),
        }
    }
}
#[derive(Deserialize)]
struct Create {
    name: String,
    description: Option<String>,
    data_type: String,
    #[serde(default, deserialize_with = "supplied")]
    visibility: Option<String>,
    options: Option<Vec<SelectOption>>,
}
#[derive(Default, Deserialize)]
struct Update {
    #[serde(default, deserialize_with = "supplied")]
    name: Option<String>,
    #[serde(
        default,
        deserialize_with = "codeyard_db_repos::repositories::records::nullable_change"
    )]
    description: Option<Option<String>>,
    #[serde(default, deserialize_with = "supplied")]
    visibility: Option<String>,
    #[serde(default, deserialize_with = "supplied")]
    options: Option<Vec<SelectOption>>,
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
    let fields = codeyard_db_repos::issue_attributes::organization_catalog(
        &state.pool,
        org.id,
        actor.user.id,
        "field",
    )
    .await
    .map_err(field_error)?;
    Ok(Json(Value::Array(
        fields
            .iter()
            .map(crate::backend::webhooks::payloads::issue_field)
            .collect(),
    )))
}
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    input: std::result::Result<Json<Create>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(&state, &name).await?;
    let Json(input) =
        input.map_err(|error| crate::backend::api::github::json_error(error).simple())?;
    if matches!(input.data_type.as_str(), "single_select" | "multi_select")
        && input.options.is_none()
    {
        return Err(Error::invalid_simple("Select fields require options"));
    }
    let field = codeyard_db_repos::issue_attributes::create_organization_field(
        &state.pool,
        org.id,
        actor.database_actor(),
        &Definition {
            id: None,
            expected_updated: None,
            kind: "field".into(),
            name: input.name,
            description: input.description,
            data_type: Some(input.data_type),
            visibility: input
                .visibility
                .unwrap_or_else(|| "organization_members_only".into()),
            is_enabled: true,
            color: None,
            options: input
                .options
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
        },
    )
    .await
    .map_err(field_error)?;
    Ok(Json(crate::backend::webhooks::payloads::issue_field(
        &field,
    )))
}
async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    input: std::result::Result<Json<Update>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(&state, &params["org"]).await?;
    let id = params["issue_field_id"]
        .parse()
        .map_err(|_| Error::missing())?;
    let Json(input) =
        input.map_err(|error| crate::backend::api::github::json_error(error).simple())?;
    let field = codeyard_db_repos::issue_attributes::patch_organization_field(
        &state.pool,
        org.id,
        actor.database_actor(),
        id,
        &FieldChanges {
            name: input.name,
            description: input.description,
            visibility: input.visibility,
            options: input
                .options
                .map(|options| options.into_iter().map(Into::into).collect()),
        },
    )
    .await
    .map_err(field_error)?;
    Ok(Json(crate::backend::webhooks::payloads::issue_field(
        &field,
    )))
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
        "field",
    )
    .await
    .map_err(field_error)?;
    Ok(StatusCode::NO_CONTENT)
}
