//! Projects v2 adapters share the owner-scoped native project operations.
mod items;
use codeyard_model::urls::api_url;

use crate::backend::api::github::pagination::Cursor;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use crate::backend::db::projects::Owner;
use crate::backend::db::projects::Project;

use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

type Params = HashMap<String, String>;

pub(super) fn project_error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<codeyard_model::project_query::Refusal>() {
        Some(codeyard_model::project_query::Refusal::Missing) => Error::missing(),
        Some(codeyard_model::project_query::Refusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Project write denied")
        }
        Some(codeyard_model::project_query::Refusal::Invalid) => {
            Error::validation("ProjectV2", "fields", "invalid")
        }
        None => Error::unavailable(error),
    }
}

async fn owner(state: &AppState, params: &Params, numeric: bool) -> Result<(Owner, String)> {
    if let Some(name) = params.get("org") {
        let org =
            crate::backend::db::organizations::profiles::get_public_organization(&state.pool, name)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        Ok((Owner::Organization(org.id), org.name))
    } else {
        let name = params
            .get("user_id")
            .or_else(|| params.get("account_id"))
            .ok_or_else(Error::missing)?;
        let user = if numeric {
            crate::backend::db::users::accounts::get_user_by_id(
                &state.pool,
                name.parse().map_err(|_| Error::missing())?,
            )
            .await
        } else {
            crate::backend::db::users::accounts::get_user_by_username(&state.pool, name).await
        }
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
        Ok((Owner::User(user.id), user.username))
    }
}

pub(super) fn project_url(project: &Project, name: &str) -> String {
    api_url(&format!(
        "/{}/{}/projectsV2/{}",
        if project.owner_kind == "org" {
            "orgs"
        } else {
            "users"
        },
        codeyard_webui::encode_component(name),
        project.number
    ))
}

pub(super) async fn document(state: &AppState, project: &Project, name: &str) -> Result<Value> {
    let creator =
        crate::backend::db::users::accounts::get_user_by_id(&state.pool, project.creator_id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    let mut owner =
        crate::backend::webhooks::payloads::person(&crate::backend::webhooks::payloads::Person {
            login: name.into(),
            id: project.owner_id,
            site_admin: false,
        });
    if project.owner_kind == "org" {
        owner["type"] = serde_json::json!("Organization");
        owner["url"] = serde_json::json!(api_url(&format!(
            "/orgs/{}",
            codeyard_webui::encode_component(name)
        )));
        owner["html_url"] = serde_json::json!(format!(
            "{}{}",
            codeyard_model::http_config::link_origin(),
            codeyard_webui::profile_path(name)
        ));
        owner["node_id"] = serde_json::json!(crate::backend::api::github::node_id::encode(
            "O",
            project.owner_id
        ));
    }
    Ok(crate::backend::webhooks::payloads::project(
        project,
        owner,
        crate::backend::webhooks::payloads::user(&creator),
    ))
}

async fn context(
    state: &AppState,
    headers: &HeaderMap,
    params: &Params,
    write: bool,
    numeric: bool,
) -> Result<(
    Project,
    String,
    Option<crate::backend::api::github::auth::Authentication>,
)> {
    let auth = crate::backend::api::github::auth::authenticate(state, headers, write).await?;
    if write {
        auth.as_ref()
            .ok_or_else(crate::backend::api::github::auth::required)?
            .require_write()?;
    }
    let (owner, name) = owner(state, params, numeric).await?;
    let number = params
        .get("project_number")
        .and_then(|s| s.parse().ok())
        .ok_or_else(Error::missing)?;
    let project = crate::backend::db::projects::get(
        &state.pool,
        auth.as_ref().map(|a| a.user.id),
        owner,
        number,
    )
    .await
    .map_err(project_error)?;
    Ok((project, name, auth))
}

#[derive(Deserialize)]
struct ListQuery {
    q: Option<String>,
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Cursor>,
    Query(query): Query<ListQuery>,
) -> Result<Response> {
    let auth = crate::backend::api::github::auth::authenticate(&state, &headers, false).await?;
    let (owner, name) = owner(&state, &params, false).await?;
    let (after, before, limit) = page.window()?;
    let query = query.q.as_deref().unwrap_or("");
    if query.len() > 4096 {
        return Err(Error::invalid("Project query is too long"));
    }
    let mut filter = crate::backend::db::projects::ListFilter::search(Some(owner), query);
    filter.after = (after > 0).then_some(after);
    filter.before = (before < i64::MAX).then_some(before);
    filter.descending = before < i64::MAX;
    let (_, mut selected) = crate::backend::db::projects::list(
        &state.pool,
        auth.as_ref().map(|a| a.user.id),
        &filter,
        0,
        (limit + 1) as i64,
    )
    .await
    .map_err(project_error)?;
    let more = page.trim(&mut selected)?;
    let (headers, _) = page.links(
        &uri,
        selected.first().map(|project| project.id),
        selected.last().map(|project| project.id),
        more,
    )?;
    let mut documents = Vec::new();
    for project in selected {
        documents.push(document(&state, &project, &name).await?);
    }
    Ok((headers, Json(documents)).into_response())
}

async fn get_project(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Json<Value>> {
    let (project, name, _) = context(&state, &headers, &params, false, false).await?;
    Ok(Json(document(&state, &project, &name).await?))
}

async fn fields(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Cursor>,
) -> Result<Response> {
    let (project, name, auth) = context(&state, &headers, &params, false, false).await?;
    let (after, before, limit) = page.window()?;
    let mut fields = crate::backend::db::projects::fields_window(
        &state.pool,
        auth.as_ref().map(|a| a.user.id),
        project.id,
        after,
        before,
        (limit + 1) as i64,
    )
    .await
    .map_err(project_error)?;
    let more = page.trim(&mut fields)?;
    let (headers, _) = page.links(
        &uri,
        fields.first().map(|field| field.id),
        fields.last().map(|field| field.id),
        more,
    )?;
    let documents = fields
        .iter()
        .map(|f| {
            crate::backend::webhooks::payloads::project_field(f, &project_url(&project, &name))
                .map_err(Error::unavailable)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((headers, Json(documents)).into_response())
}

async fn get_field(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Json<Value>> {
    let (project, name, auth) = context(&state, &headers, &params, false, false).await?;
    let id: i64 = params["field_id"].parse().map_err(|_| Error::missing())?;
    let field = crate::backend::db::projects::fields(
        &state.pool,
        auth.as_ref().map(|a| a.user.id),
        project.id,
    )
    .await
    .map_err(project_error)?
    .into_iter()
    .find(|f| f.id == id)
    .ok_or_else(Error::missing)?;
    Ok(Json(
        crate::backend::webhooks::payloads::project_field(&field, &project_url(&project, &name))
            .map_err(Error::unavailable)?,
    ))
}

async fn add_field(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    input: std::result::Result<
        Json<crate::backend::db::projects::NewField>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Result<Response> {
    let (project, name, auth) = context(&state, &headers, &params, true, false).await?;
    let Json(input) = input.map_err(crate::backend::api::github::json_error)?;
    let auth = auth.ok_or_else(crate::backend::api::github::auth::required)?;
    let field = crate::backend::db::projects::add_field(
        &state.pool,
        auth.user.id,
        &auth.credential(),
        project.id,
        &input,
    )
    .await
    .map_err(project_error)?;
    Ok((
        StatusCode::CREATED,
        Json(
            crate::backend::webhooks::payloads::project_field(
                &field,
                &project_url(&project, &name),
            )
            .map_err(Error::unavailable)?,
        ),
    )
        .into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = items::handlers();
    handlers.extend([
        ("projects/list-for-org", get(list)),
        ("projects/list-for-user", get(list)),
        ("projects/get-for-org", get(get_project)),
        ("projects/get-for-user", get(get_project)),
        ("projects/list-fields-for-org", get(fields)),
        ("projects/list-fields-for-user", get(fields)),
        ("projects/get-field-for-org", get(get_field)),
        ("projects/get-field-for-user", get(get_field)),
        ("projects/add-field-for-org", post(add_field)),
        ("projects/add-field-for-user", post(add_field)),
    ]);
    handlers
}
