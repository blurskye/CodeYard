//! JWT-authorized app webhook configuration, history and real background redelivery.
use super::{
    super::{api_url, Error, Result},
    installations::jwt,
};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::github_app_hooks::Delivery;
use crate::backend::db::webhooks::WebhookPatch;
use crate::backend::webhooks;
use crate::backend::webhooks::settings;
use crate::backend::webhooks::settings::HookError;
use crate::backend::webhooks::settings::HookForm;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use codeyard_db_core::models::Webhook;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::Arc;
fn hook_error(error: HookError) -> Error {
    match error {
        HookError::Invalid(message) => Error::invalid(message),
        HookError::NotFound => Error::missing(),
        HookError::Denied => Error::new(StatusCode::FORBIDDEN, "Webhook access denied"),
        HookError::Unavailable => Error::unavailable(()),
    }
}
fn configuration(hook: Option<&Webhook>) -> Value {
    match hook {
        Some(hook) => {
            json!({"url":hook.payload_url,"content_type":if hook.content_type==settings::JSON_CONTENT_TYPE{"json"}else{"form"},"insecure_ssl":if hook.ssl_verify{"0"}else{"1"}})
        }
        None => json!({"content_type":"form","insecure_ssl":"0"}),
    }
}
pub(super) async fn get_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let app = jwt(&state, &headers).await?;
    let hook = db::github_app_hooks::hook(&state.pool, app.id)
        .await
        .map_err(Error::unavailable)?;
    Ok(Json(configuration(hook.as_ref())))
}
pub(super) async fn update_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    let app = jwt(&state, &headers).await?;
    if !body.is_object() {
        return Err(Error::invalid("Expected a webhook configuration object"));
    }
    let string = |key: &str| -> Result<Option<&str>> {
        body.get(key)
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| Error::invalid(format!("Invalid {key}")))
            })
            .transpose()
    };
    let url = string("url")?;
    let content = match string("content_type")? {
        Some("json") => Some(settings::JSON_CONTENT_TYPE),
        Some("form") => Some(settings::FORM_CONTENT_TYPE),
        Some(_) => return Err(Error::invalid("Choose json or form")),
        None => None,
    };
    let ssl = match body.get("insecure_ssl") {
        None => None,
        Some(Value::String(value)) if value == "0" || value == "1" => Some(value == "0"),
        Some(Value::Number(value)) if value.as_i64() == Some(0) || value.as_i64() == Some(1) => {
            Some(value.as_i64() == Some(0))
        }
        _ => return Err(Error::invalid("insecure_ssl must be 0 or 1")),
    };
    let secret = string("secret")?;
    // The stored hook fills the fields the request leaves out only for validation. The save
    // writes just the named fields, so a concurrent edit of another field stands.
    let old = db::github_app_hooks::hook(&state.pool, app.id)
        .await
        .map_err(Error::unavailable)?;
    let creating = old.is_none();
    let form = HookForm {
        username: None,
        password: None,
        clear_password: false,
        clear_secret: false,
        payload_url: url
            .or_else(|| old.as_ref().map(|h| h.payload_url.as_str()))
            .ok_or_else(|| Error::validation("Hook", "url", "missing_field"))?
            .into(),
        content_type: content
            .or_else(|| old.as_ref().map(|h| h.content_type.as_str()))
            .unwrap_or(settings::FORM_CONTENT_TYPE)
            .into(),
        events: vec!["*".into()],
        active: true,
        ssl_verify: ssl.unwrap_or(true),
        secret: secret.unwrap_or("").into(),
    };
    let fields = settings::validated(&form).map_err(hook_error)?;
    let patch = WebhookPatch {
        payload_url: (creating || url.is_some()).then_some(fields.payload_url),
        content_type: (creating || content.is_some()).then_some(fields.content_type),
        events: Some(app.events),
        active: creating.then_some(true),
        ssl_verify: (creating || ssl.is_some()).then_some(fields.ssl_verify),
        ..WebhookPatch::default()
    };
    let sealed =
        settings::seal(state.secret_encryptor.as_deref(), &form.secret).map_err(hook_error)?;
    if !db::github_app_hooks::save(
        &state.pool,
        app.id,
        &patch,
        sealed.as_ref(),
        secret == Some(""),
    )
    .await
    .map_err(Error::unavailable)?
    {
        return Err(Error::validation("Hook", "url", "missing_field"));
    }
    let hook = db::github_app_hooks::hook(&state.pool, app.id)
        .await
        .map_err(Error::unavailable)?;
    Ok(Json(configuration(hook.as_ref())))
}
fn headers(value: &str) -> Value {
    Value::Object(
        value
            .lines()
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_owned(), Value::String(value.trim().into())))
            .collect::<Map<_, _>>(),
    )
}
fn document(delivery: &Delivery, detail: bool) -> Value {
    let record = &delivery.recorded;
    let code = record.status_code.unwrap_or(0);
    let mut value = json!({"id":record.id,"guid":record.guid,"delivered_at":record.delivered_at,"redelivery":delivery.redelivery,"duration":f64::from(record.duration_ms)/1000.0,"status":record.error.clone().unwrap_or_else(||StatusCode::from_u16(code.try_into().unwrap_or(0)).ok().and_then(|status|status.canonical_reason().map(str::to_owned)).unwrap_or_else(||"Unknown response".into())),"status_code":code,"event":record.event,"action":record.action,"installation_id":delivery.installation_id,"repository_id":delivery.repository_id});
    if detail {
        let outgoing = webhooks::delivery::redelivery(record);
        value["url"] = json!(delivery.url);
        value["request"] = json!({"headers":headers(&record.request_headers),"payload":webhooks::delivery::json_payload(&outgoing).filter(Value::is_object)});
        value["response"] = json!({"headers":record.response_headers.as_deref().map(headers),"payload":record.response_body});
    }
    value
}
#[derive(Deserialize)]
pub(super) struct List {
    per_page: Option<usize>,
    cursor: Option<String>,
    status: Option<String>,
}
pub(super) async fn list_deliveries(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<List>,
) -> Result<Response> {
    let app = jwt(&state, &headers).await?;
    if query
        .status
        .as_deref()
        .is_some_and(|status| !matches!(status, "success" | "failure"))
    {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "Invalid delivery status",
        ));
    }
    let mut all = db::github_app_hooks::deliveries(&state.pool, app.id)
        .await
        .map_err(Error::unavailable)?;
    all.retain(|delivery| match query.status.as_deref() {
        Some("success") => delivery.recorded.succeeded(),
        Some("failure") => !delivery.recorded.succeeded(),
        _ => true,
    });
    let cursor = query
        .cursor
        .as_ref()
        .map(|cursor| -> Result<(bool, i64)> {
            let cursor = URL_SAFE_NO_PAD
                .decode(cursor)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .ok_or_else(|| Error::new(StatusCode::BAD_REQUEST, "Invalid delivery cursor"))?;
            let (direction, id) = cursor
                .split_once(':')
                .ok_or_else(|| Error::new(StatusCode::BAD_REQUEST, "Invalid delivery cursor"))?;
            if !matches!(direction, "before" | "after") {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    "Invalid delivery cursor",
                ));
            }
            Ok((
                direction == "after",
                id.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| {
                    Error::new(StatusCode::BAD_REQUEST, "Invalid delivery cursor")
                })?,
            ))
        })
        .transpose()?;
    let limit = super::super::pagination::page_size(query.per_page);
    let mut rows = all
        .iter()
        .filter(|delivery| {
            cursor.is_none_or(|(after, id)| {
                if after {
                    delivery.recorded.id > id
                } else {
                    delivery.recorded.id < id
                }
            })
        })
        .collect::<Vec<_>>();
    if cursor.is_some_and(|(after, _)| after) {
        rows = rows.into_iter().rev().take(limit).collect();
        rows.reverse();
    } else {
        rows.truncate(limit);
    }
    let mut links = vec![];
    let link = |direction: &str, id: i64, relation: &str| {
        format!(
            "<{}?per_page={limit}&cursor={}{}>; rel=\"{relation}\"",
            api_url("/app/hook/deliveries"),
            URL_SAFE_NO_PAD.encode(format!("{direction}:{id}")),
            query
                .status
                .as_ref()
                .map(|s| format!("&status={s}"))
                .unwrap_or_default()
        )
    };
    if let Some(first) = rows.first() {
        if all.iter().any(|r| r.recorded.id > first.recorded.id) {
            links.push(link("after", first.recorded.id, "prev"));
        }
    }
    if let Some(last) = rows.last() {
        if all.iter().any(|r| r.recorded.id < last.recorded.id) {
            links.push(link("before", last.recorded.id, "next"));
        }
    }
    let mut headers = HeaderMap::new();
    if !links.is_empty() {
        headers.insert(
            header::LINK,
            links.join(", ").parse().map_err(Error::unavailable)?,
        );
    }
    Ok((
        headers,
        Json(
            rows.into_iter()
                .map(|delivery| document(delivery, false))
                .collect::<Vec<_>>(),
        ),
    )
        .into_response())
}
pub(super) async fn get_delivery(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    let app = jwt(&state, &headers).await?;
    let record = db::github_app_hooks::delivery(&state.pool, app.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(Json(document(&record, true)))
}
pub(super) async fn redeliver(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<(StatusCode, Json<Value>)> {
    let app = jwt(&state, &headers).await?;
    let hook = db::github_app_hooks::hook(&state.pool, app.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let record = db::github_app_hooks::delivery(&state.pool, app.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let mut outgoing = webhooks::delivery::redelivery(&record.recorded);
    outgoing.guid = record.recorded.guid;
    if !webhooks::enqueue(&state.pool, hook, outgoing) {
        return Err(Error::unavailable(()));
    }
    Ok((StatusCode::ACCEPTED, Json(json!({}))))
}
