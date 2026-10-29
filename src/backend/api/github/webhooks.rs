//! Repository hooks share validation, encrypted secrets and delivery with the settings page.
use super::{auth, pagination::Pagination, repo_url, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        models::{User, Webhook, WebhookDelivery},
        webhooks::{HookOwner, HookSettings},
    },
    webhooks::{
        payloads,
        settings::{self, HookError, HookForm, HookPatch},
    },
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

type Params = HashMap<String, String>;

fn hook_error(error: HookError) -> Error {
    match error {
        HookError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Repository administration required")
        }
        HookError::NotFound => Error::missing(),
        HookError::Invalid(message) => Error::invalid(message),
        HookError::Unavailable => Error::unavailable(()),
    }
}

async fn scope(
    state: &AppState,
    headers: &HeaderMap,
    params: &Params,
    write: bool,
) -> Result<(HookOwner, User)> {
    let (repo, actor) =
        repository(state, headers, &params["owner"], &params["repo"], write).await?;
    let actor = actor.ok_or_else(auth::required)?.user;
    let owner = HookOwner::Repository(repo.id);
    settings::authorize(&state.pool, &actor, owner)
        .await
        .map_err(hook_error)?;
    Ok((owner, actor))
}

fn id(params: &Params, key: &str) -> Result<i64> {
    params
        .get(key)
        .and_then(|id| id.parse().ok())
        .ok_or_else(Error::missing)
}

/// Reads a hook with its delivery history in the transaction that checks the actor may manage it.
async fn snapshot(
    state: &AppState,
    owner: HookOwner,
    actor: &User,
    hook: i64,
    delivery: Option<i64>,
) -> Result<(Webhook, HookSettings)> {
    let mut settings =
        db::webhooks::read_settings(&state.pool, owner, actor.id, Some(hook), delivery, 0, false)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(|| {
                Error::new(StatusCode::FORBIDDEN, "Repository administration required")
            })?;
    let hook = settings.hook.take().ok_or_else(Error::missing)?;
    Ok((hook, settings))
}

async fn read_hook(
    state: &AppState,
    params: &Params,
    owner: HookOwner,
    actor: &User,
    hook: i64,
) -> Result<Value> {
    let (hook, settings) = snapshot(state, owner, actor, hook, None).await?;
    Ok(document(params, &hook, settings.deliveries.first()))
}

fn document(params: &Params, hook: &Webhook, last: Option<&WebhookDelivery>) -> Value {
    let url = format!(
        "{}/hooks/{}",
        repo_url(&params["owner"], &params["repo"]),
        hook.id
    );
    let mut value = payloads::webhook(hook, &url);
    if hook.encrypted_secret.is_none() {
        value["config"]
            .as_object_mut()
            .expect("webhook config")
            .remove("secret");
    }
    value["test_url"] = json!(format!("{url}/tests"));
    value["last_response"] = json!({"code": last.and_then(|row| row.status_code),
        "status": last.map(|row| if row.succeeded() { "active" } else { "failed" }).unwrap_or("unused"),
        "message": last.and_then(|row| row.error.as_deref())});
    value
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (owner, _) = scope(&state, &headers, &params, false).await?;
    let hooks = db::webhooks::list_webhooks(&state.pool, owner)
        .await
        .map_err(Error::unavailable)?;
    let (offset, limit) = page.window()?;
    let next = hooks.len() > offset + limit;
    let mut values = Vec::new();
    for hook in hooks.iter().skip(offset).take(limit) {
        let deliveries = db::webhooks::list_delivery_summaries(&state.pool, hook.id)
            .await
            .map_err(Error::unavailable)?;
        values.push(document(&params, hook, deliveries.first()));
    }
    page.response(&uri, values, next)
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    OriginalUri(uri): OriginalUri,
) -> Result<Json<Value>> {
    let (owner, actor) = scope(&state, &headers, &params, false).await?;
    let value = read_hook(&state, &params, owner, &actor, id(&params, "hook_id")?).await?;
    Ok(Json(if uri.path().ends_with("/config") {
        value["config"].clone()
    } else {
        value
    }))
}

#[derive(Default, Deserialize)]
struct Config {
    url: Option<String>,
    content_type: Option<String>,
    secret: Option<String>,
    insecure_ssl: Option<Value>,
}

#[derive(Deserialize)]
struct Write {
    name: Option<String>,
    config: Option<Config>,
    events: Option<Vec<String>>,
    add_events: Option<Vec<String>>,
    remove_events: Option<Vec<String>>,
    active: Option<bool>,
}

fn ssl(value: Value) -> Result<bool> {
    match value {
        Value::String(value) if value == "0" || value == "1" => Ok(value == "0"),
        Value::Number(value) if value == 0.into() || value == 1.into() => Ok(value == 0.into()),
        _ => Err(Error::invalid("insecure_ssl must be 0 or 1")),
    }
}

fn change_events(events: &mut Vec<String>, add: Option<Vec<String>>, remove: Option<Vec<String>>) {
    for event in add.unwrap_or_default() {
        if !events.contains(&event) {
            events.push(event);
        }
    }
    if let Some(remove) = remove {
        events.retain(|event| !remove.contains(event));
    }
}

async fn save(
    state: Arc<AppState>,
    headers: HeaderMap,
    params: Params,
    input: Write,
    config_only: bool,
) -> Result<Response> {
    let (owner, actor) = scope(&state, &headers, &params, true).await?;
    if input.name.as_deref().is_some_and(|name| name != "web") {
        return Err(Error::invalid("name must be web"));
    }
    let hook_id = params
        .get("hook_id")
        .map(|_| id(&params, "hook_id"))
        .transpose()?;
    let config = input.config.unwrap_or_default();
    let content_type = match config.content_type.as_deref() {
        Some("json") => Some(settings::JSON_CONTENT_TYPE.to_owned()),
        Some("form") => Some(settings::FORM_CONTENT_TYPE.to_owned()),
        None => None,
        _ => return Err(Error::invalid("content_type must be json or form")),
    };
    let ssl_verify = config.insecure_ssl.map(ssl).transpose()?;
    let id = match hook_id {
        None => {
            let mut events = input.events.unwrap_or_else(|| vec!["push".into()]);
            change_events(&mut events, input.add_events, input.remove_events);
            let form = HookForm {
                username: None,
                password: None,
                clear_password: false,
                clear_secret: false,
                payload_url: config
                    .url
                    .ok_or_else(|| Error::invalid("config.url is required"))?,
                content_type: content_type
                    .unwrap_or_else(|| settings::FORM_CONTENT_TYPE.to_owned()),
                events,
                active: input.active.unwrap_or(true),
                ssl_verify: ssl_verify.unwrap_or(true),
                secret: config.secret.unwrap_or_default(),
            };
            settings::save(
                &state.pool,
                state.secret_encryptor.as_deref(),
                owner,
                &actor,
                None,
                &form,
            )
            .await
            .map_err(hook_error)?
        }
        Some(id) => {
            // Only the supplied fields are written, so concurrent edits to the others survive.
            let events = if input.add_events.is_some() || input.remove_events.is_some() {
                let mut events = match input.events {
                    Some(events) => events,
                    None => snapshot(&state, owner, &actor, id, None)
                        .await?
                        .0
                        .events
                        .split(',')
                        .map(str::to_owned)
                        .collect(),
                };
                change_events(&mut events, input.add_events, input.remove_events);
                Some(events)
            } else {
                input.events
            };
            let patch = HookPatch {
                payload_url: config.url,
                content_type,
                events,
                active: input.active,
                ssl_verify,
                secret: config.secret,
                ..Default::default()
            };
            settings::update(
                &state.pool,
                state.secret_encryptor.as_deref(),
                owner,
                &actor,
                id,
                &patch,
            )
            .await
            .map_err(hook_error)?;
            id
        }
    };
    let value = read_hook(&state, &params, owner, &actor, id).await?;
    let value = if config_only {
        value["config"].clone()
    } else {
        value
    };
    Ok((
        if hook_id.is_none() {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(value),
    )
        .into_response())
}

async fn write(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Json(input): Json<Write>,
) -> Result<Response> {
    save(state, headers, params, input, false).await
}
async fn write_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Json(config): Json<Config>,
) -> Result<Response> {
    save(
        state,
        headers,
        params,
        Write {
            name: None,
            config: Some(config),
            events: None,
            add_events: None,
            remove_events: None,
            active: None,
        },
        true,
    )
    .await
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<StatusCode> {
    let (owner, actor) = scope(&state, &headers, &params, true).await?;
    settings::delete(&state.pool, owner, &actor, id(&params, "hook_id")?)
        .await
        .map_err(hook_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub(super) struct Deliveries {
    per_page: Option<usize>,
    cursor: Option<String>,
    status: Option<String>,
}

/// GitHub pages delivery history newest first by cursor, optionally keeping only successes
/// or only failures.
pub(super) fn delivery_page(
    rows: Vec<WebhookDelivery>,
    query: Deliveries,
    uri: &Uri,
) -> Result<(HeaderMap, Vec<WebhookDelivery>)> {
    let cursor = query
        .cursor
        .as_ref()
        .map(|cursor| {
            cursor
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| Error::validation("Hook", "cursor", "invalid"))
        })
        .transpose()?;
    if query
        .status
        .as_deref()
        .is_some_and(|status| !matches!(status, "success" | "failure"))
    {
        return Err(Error::validation("Hook", "status", "invalid"));
    }
    let limit = query.per_page.unwrap_or(30).clamp(1, 100);
    let mut rows = rows
        .into_iter()
        .filter(|row| cursor.is_none_or(|cursor| row.id < cursor))
        .filter(|row| match query.status.as_deref() {
            Some("success") => row
                .status_code
                .is_some_and(|code| (200..400).contains(&code)),
            Some("failure") => row
                .status_code
                .is_some_and(|code| (400..600).contains(&code)),
            _ => true,
        })
        .collect::<Vec<_>>();
    let next = rows.len() > limit;
    rows.truncate(limit);
    let mut headers = HeaderMap::new();
    if next {
        let mut pairs = vec![
            ("per_page", limit.to_string()),
            ("cursor", rows.last().unwrap().id.to_string()),
        ];
        if let Some(status) = query.status {
            pairs.push(("status", status));
        }
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        headers.insert(
            header::LINK,
            format!(
                "<{}{}?{query}>; rel=\"next\"",
                crate::backend::api::http::config::link_origin(),
                uri.path()
            )
            .parse()
            .map_err(Error::unavailable)?,
        );
    }
    Ok((headers, rows))
}

fn delivery_document(hook: &Webhook, row: &WebhookDelivery, detail: bool) -> Value {
    let mut value = payloads::webhook_delivery(row, detail);
    value["repository_id"] = json!(hook.repo_id);
    value
}

async fn deliveries(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(query): Query<Deliveries>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (owner, actor) = scope(&state, &headers, &params, false).await?;
    let (hook, settings) = snapshot(&state, owner, &actor, id(&params, "hook_id")?, None).await?;
    let (headers, rows) = delivery_page(settings.deliveries, query, &uri)?;
    let rows = rows
        .iter()
        .map(|row| delivery_document(&hook, row, false))
        .collect::<Vec<_>>();
    Ok((headers, Json(rows)).into_response())
}

async fn delivery(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Json<Value>> {
    let (owner, actor) = scope(&state, &headers, &params, false).await?;
    let (hook, settings) = snapshot(
        &state,
        owner,
        &actor,
        id(&params, "hook_id")?,
        Some(id(&params, "delivery_id")?),
    )
    .await?;
    let row = settings.delivery.ok_or_else(Error::missing)?;
    Ok(Json(delivery_document(&hook, &row, true)))
}

async fn trigger(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (owner, actor) = scope(&state, &headers, &params, true).await?;
    let hook = id(&params, "hook_id")?;
    if params.contains_key("delivery_id") {
        settings::redeliver(
            &state.pool,
            owner,
            &actor,
            hook,
            id(&params, "delivery_id")?,
        )
        .await
        .map_err(hook_error)?;
        return Ok((StatusCode::ACCEPTED, Json(json!({}))).into_response());
    }
    if uri.path().ends_with("/tests") {
        settings::test_push(&state.pool, state.cluster.as_ref(), owner, &actor, hook)
            .await
            .map_err(hook_error)?;
    } else {
        settings::ping(&state.pool, owner, &actor, hook)
            .await
            .map_err(hook_error)?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-webhooks", get(list)),
        ("repos/get-webhook", get(read)),
        ("repos/create-webhook", post(write)),
        ("repos/update-webhook", patch(write)),
        ("repos/delete-webhook", delete(remove)),
        ("repos/get-webhook-config-for-repo", get(read)),
        ("repos/update-webhook-config-for-repo", patch(write_config)),
        ("repos/list-webhook-deliveries", get(deliveries)),
        ("repos/get-webhook-delivery", get(delivery)),
        ("repos/redeliver-webhook-delivery", post(trigger)),
        ("repos/ping-webhook", post(trigger)),
        ("repos/test-push-webhook", post(trigger)),
    ]
}
