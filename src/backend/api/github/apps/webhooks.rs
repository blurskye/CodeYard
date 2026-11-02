//! JWT-authorized app webhook configuration, history and real background redelivery.
use super::{
    super::{api_url, Error, Result},
    installations::jwt,
};
use crate::backend::{
    app::AppState,
    db::{self, github_app_hooks::Delivery, models::Webhook},
    webhooks::{
        self,
        settings::{self, HookError, HookForm},
    },
};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
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
    let old = db::github_app_hooks::hook(&state.pool, app.id)
        .await
        .map_err(Error::unavailable)?;
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
    let content = match string("content_type")? {
        Some("json") => settings::JSON_CONTENT_TYPE,
        Some("form") => settings::FORM_CONTENT_TYPE,
        Some(_) => return Err(Error::invalid("Choose json or form")),
        None => old
            .as_ref()
            .map_or(settings::FORM_CONTENT_TYPE, |h| h.content_type.as_str()),
    };
    let ssl = match body.get("insecure_ssl") {
        None => old.as_ref().is_none_or(|h| h.ssl_verify),
        Some(Value::String(value)) if value == "0" || value == "1" => value == "0",
        Some(Value::Number(value)) if value.as_i64() == Some(0) || value.as_i64() == Some(1) => {
            value.as_i64() == Some(0)
        }
        _ => return Err(Error::invalid("insecure_ssl must be 0 or 1")),
    };
    let secret = string("secret")?;
    let form = HookForm {
        username: None,
        password: None,
        clear_password: false,
        clear_secret: false,
        payload_url: string("url")?
            .or_else(|| old.as_ref().map(|h| h.payload_url.as_str()))
            .ok_or_else(|| Error::validation("Hook", "url", "missing_field"))?
            .into(),
        content_type: content.into(),
        events: vec!["*".into()],
        active: old.as_ref().is_none_or(|hook| hook.active),
        ssl_verify: ssl,
        secret: secret.unwrap_or("").into(),
    };
    let mut fields = settings::validated(&form).map_err(hook_error)?;
    fields.events = app.events;
    let sealed =
        settings::seal(state.secret_encryptor.as_deref(), &form.secret).map_err(hook_error)?;
    db::github_app_hooks::save(
        &state.pool,
        app.id,
        &fields,
        sealed.as_ref(),
        secret == Some(""),
    )
    .await
    .map_err(Error::unavailable)?;
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
        Some("success") => delivery
            .recorded
            .status_code
            .is_some_and(|code| (200..400).contains(&code)),
        Some("failure") => delivery
            .recorded
            .status_code
            .is_some_and(|code| (400..600).contains(&code)),
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
    let limit = query.per_page.unwrap_or(30).clamp(1, 100);
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
