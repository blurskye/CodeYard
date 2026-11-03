//! Organization hooks share validation, encrypted secrets and sends with native settings.
use super::{auth, target, AppState, Error, Result};
use crate::backend::{
    api::github::{
        api_url,
        pagination::Pagination,
        webhooks::{delivery_page, Deliveries},
    },
    db::{
        models::{Organization, Webhook},
        webhooks::{self as hooks, DeliveryWindow, HookOwner, HookSettings},
    },
    webhooks::{
        payloads,
        settings::{self, HookError, HookForm, HookPatch},
    },
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, MethodRouter},
    Json,
};
use serde_json::{Map, Value};
use std::{collections::HashMap, sync::Arc};
type Params = HashMap<String, String>;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/list-webhooks", get(list)),
        ("orgs/create-webhook", post(create)),
        ("orgs/get-webhook", get(get_hook)),
        ("orgs/update-webhook", patch(update)),
        ("orgs/delete-webhook", delete(remove)),
        ("orgs/get-webhook-config-for-org", get(get_config)),
        ("orgs/update-webhook-config-for-org", patch(update_config)),
        ("orgs/list-webhook-deliveries", get(deliveries)),
        ("orgs/get-webhook-delivery", get(delivery)),
        ("orgs/redeliver-webhook-delivery", post(redeliver)),
        ("orgs/ping-webhook", post(ping)),
    ]
}
fn error(error: HookError) -> Error {
    super::super::webhooks::owner_error(error, "Must be an organization owner")
}
fn id(params: &Params, name: &str) -> Result<i64> {
    params
        .get(name)
        .and_then(|id| id.parse().ok())
        .filter(|id| *id > 0)
        .ok_or_else(Error::missing)
}
async fn actor(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    write: bool,
) -> Result<(Organization, auth::Authentication)> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    if write {
        actor.require_write()?;
    }
    Ok((target(state, name).await?, actor))
}
async fn snapshot(
    state: &AppState,
    org: i64,
    actor: &auth::Authentication,
    hook: i64,
    delivery: Option<i64>,
    history: Option<DeliveryWindow>,
) -> Result<HookSettings> {
    let settings = hooks::read_settings(
        &state.pool,
        HookOwner::Organization(org),
        actor.user.id,
        &actor.credential(),
        Some(hook),
        delivery,
        0,
        history.unwrap_or_default(),
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(|| error(HookError::Denied))?;
    if settings.hook.is_none() {
        return Err(Error::missing());
    }
    Ok(settings)
}
fn url(org: &str, id: i64) -> String {
    api_url(&format!(
        "/orgs/{}/hooks/{id}",
        codeyard_webui::encode_component(org)
    ))
}
fn document(org: &str, hook: &Webhook) -> Value {
    payloads::webhook(hook, &url(org, hook.id))
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (org, actor) = actor(&state, &headers, &name, false).await?;
    let (offset, limit) = page.window()?;
    let rows = hooks::list_managed_webhooks(
        &state.pool,
        HookOwner::Organization(org.id),
        actor.user.id,
        &actor.credential(),
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(|| error(HookError::Denied))?;
    let next = rows.len() > limit;
    page.response(
        &uri,
        rows.iter().map(|hook| document(&org.name, hook)).collect(),
        next,
    )
}
async fn get_hook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Json<Value>> {
    let (org, actor) = actor(&state, &headers, &params["org"], false).await?;
    let snapshot = snapshot(&state, org.id, &actor, id(&params, "hook_id")?, None, None).await?;
    Ok(Json(document(&org.name, snapshot.hook.as_ref().unwrap())))
}
async fn get_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Json<Value>> {
    let (org, actor) = actor(&state, &headers, &params["org"], false).await?;
    let snapshot = snapshot(&state, org.id, &actor, id(&params, "hook_id")?, None, None).await?;
    Ok(Json(payloads::webhook_config(
        snapshot.hook.as_ref().unwrap(),
    )))
}
fn object(value: &Value) -> Result<&Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| Error::validation("Hook", "body", "invalid"))
}
fn text(object: &Map<String, Value>, name: &str) -> Result<Option<String>> {
    match object.get(name) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        _ => Err(Error::validation("Hook", name, "invalid")),
    }
}
fn boolean(object: &Map<String, Value>, name: &str) -> Result<Option<bool>> {
    object
        .get(name)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| Error::validation("Hook", name, "invalid"))
        })
        .transpose()
}
fn config(value: Option<&Value>) -> Result<HookPatch> {
    let Some(value) = value else {
        return Ok(HookPatch::default());
    };
    let config = object(value)?;
    let content_type = text(config, "content_type")?
        .map(|kind| match kind.as_str() {
            "json" => Ok(settings::JSON_CONTENT_TYPE.to_owned()),
            "form" => Ok(settings::FORM_CONTENT_TYPE.to_owned()),
            _ => Err(Error::validation("Hook", "content_type", "invalid")),
        })
        .transpose()?;
    let ssl_verify = config
        .get("insecure_ssl")
        .map(|value| match (value.as_str(), value.as_i64()) {
            (Some("0"), _) | (_, Some(0)) => Ok(true),
            (Some("1"), _) | (_, Some(1)) => Ok(false),
            _ => Err(Error::validation("Hook", "insecure_ssl", "invalid")),
        })
        .transpose()?;
    Ok(HookPatch {
        username: text(config, "username")?,
        password: text(config, "password")?,
        payload_url: text(config, "url")?,
        content_type,
        secret: text(config, "secret")?,
        ssl_verify,
        ..Default::default()
    })
}
fn parse_patch(value: &Value) -> Result<HookPatch> {
    let object = object(value)?;
    if text(object, "name")?.is_some_and(|name| name != "web") {
        return Err(Error::validation("Hook", "name", "invalid"));
    }
    let mut patch = config(object.get("config"))?;
    // A full-hook update replaces the configuration it sends, so an omitted secret is cleared;
    // without a `config` object it keeps the stored configuration, secret included.
    if object.get("config").is_some() {
        patch.secret.get_or_insert_with(String::new);
    }
    patch.active = boolean(object, "active")?;
    patch.events = object
        .get("events")
        .map(|value| {
            serde_json::from_value::<Vec<String>>(value.clone())
                .map_err(|_| Error::validation("Hook", "events", "invalid"))
        })
        .transpose()?;
    Ok(patch)
}
type Body = std::result::Result<Json<Value>, axum::extract::rejection::JsonRejection>;
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Body,
) -> Result<Response> {
    let (org, actor) = actor(&state, &headers, &name, true).await?;
    let Json(value) = body.map_err(super::super::json_error)?;
    let object = object(&value)?;
    if text(object, "name")?.as_deref() != Some("web") {
        return Err(Error::validation("Hook", "name", "invalid"));
    }
    let patch = parse_patch(&value)?;
    let input = HookForm {
        username: patch.username,
        password: patch.password,
        clear_password: false,
        payload_url: patch
            .payload_url
            .ok_or_else(|| Error::validation("Hook", "url", "missing_field"))?,
        content_type: patch
            .content_type
            .unwrap_or_else(|| settings::FORM_CONTENT_TYPE.into()),
        events: patch.events.unwrap_or_else(|| vec!["push".into()]),
        active: patch.active.unwrap_or(true),
        ssl_verify: patch.ssl_verify.unwrap_or(true),
        secret: patch.secret.unwrap_or_default(),
        clear_secret: false,
    };
    let id = settings::save(
        &state.pool,
        state.secret_encryptor.as_deref(),
        HookOwner::Organization(org.id),
        &actor.user,
        &actor.credential(),
        None,
        &input,
    )
    .await
    .map_err(error)?;
    let hook = snapshot(&state, org.id, &actor, id, None, None)
        .await?
        .hook
        .unwrap();
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, url(&org.name, id))],
        Json(document(&org.name, &hook)),
    )
        .into_response())
}
async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    body: Body,
) -> Result<Json<Value>> {
    let (org, actor) = actor(&state, &headers, &params["org"], true).await?;
    let Json(value) = body.map_err(super::super::json_error)?;
    let patch = parse_patch(&value)?;
    let id = id(&params, "hook_id")?;
    settings::update(
        &state.pool,
        state.secret_encryptor.as_deref(),
        HookOwner::Organization(org.id),
        &actor.user,
        &actor.credential(),
        id,
        &patch,
    )
    .await
    .map_err(error)?;
    Ok(Json(document(
        &org.name,
        &snapshot(&state, org.id, &actor, id, None, None)
            .await?
            .hook
            .unwrap(),
    )))
}
async fn update_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    body: Body,
) -> Result<Json<Value>> {
    let (org, actor) = actor(&state, &headers, &params["org"], true).await?;
    let Json(value) = body.map_err(super::super::json_error)?;
    let patch = config(Some(&value))?;
    let id = id(&params, "hook_id")?;
    settings::update(
        &state.pool,
        state.secret_encryptor.as_deref(),
        HookOwner::Organization(org.id),
        &actor.user,
        &actor.credential(),
        id,
        &patch,
    )
    .await
    .map_err(error)?;
    Ok(Json(payloads::webhook_config(
        &snapshot(&state, org.id, &actor, id, None, None)
            .await?
            .hook
            .unwrap(),
    )))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<StatusCode> {
    let (org, actor) = actor(&state, &headers, &params["org"], true).await?;
    settings::delete(
        &state.pool,
        HookOwner::Organization(org.id),
        &actor.user,
        &actor.credential(),
        id(&params, "hook_id")?,
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn ping(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<StatusCode> {
    let (org, actor) = actor(&state, &headers, &params["org"], true).await?;
    settings::ping(
        &state.pool,
        HookOwner::Organization(org.id),
        &actor.user,
        &actor.credential(),
        id(&params, "hook_id")?,
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn redeliver(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<StatusCode> {
    let (org, actor) = actor(&state, &headers, &params["org"], true).await?;
    settings::redeliver(
        &state.pool,
        HookOwner::Organization(org.id),
        &actor.user,
        &actor.credential(),
        id(&params, "hook_id")?,
        id(&params, "delivery_id")?,
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::ACCEPTED)
}
async fn delivery(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Json<Value>> {
    let (org, actor) = actor(&state, &headers, &params["org"], false).await?;
    let snapshot = snapshot(
        &state,
        org.id,
        &actor,
        id(&params, "hook_id")?,
        Some(id(&params, "delivery_id")?),
        None,
    )
    .await?;
    Ok(Json(payloads::webhook_delivery(
        &snapshot.delivery.ok_or_else(Error::missing)?,
        true,
    )))
}
async fn deliveries(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(query): Query<Deliveries>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (org, actor) = actor(&state, &headers, &params["org"], false).await?;
    let window = query.window(true)?;
    let snapshot = snapshot(
        &state,
        org.id,
        &actor,
        id(&params, "hook_id")?,
        None,
        Some(window),
    )
    .await?;
    let (headers, rows) = delivery_page(snapshot.deliveries, query, &uri)?;
    Ok((
        headers,
        Json(
            rows.iter()
                .map(|row| payloads::webhook_delivery(row, false))
                .collect::<Vec<_>>(),
        ),
    )
        .into_response())
}
