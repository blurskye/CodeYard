//! The webhook section of a settings page: the view model it renders, built after the viewer
//! has been checked, and the form fields it posts back as a hook draft.
use super::events;
use super::settings::{self, HookError, HookForm, JSON_CONTENT_TYPE};
use crate::backend::crypto::SecretEncryptor;
use crate::backend::db::models::{User, Webhook, WebhookDelivery};
use crate::backend::db::webhooks::{self as hooks, HookOwner};
use crate::backend::db::DbPool;
use crate::webui::components::webhooks::{
    DeliveryDetail, DeliveryRow, HookDetail, HookDraft, HookRow, WebhookPanel,
};
use std::collections::HashMap;

/// Where the section's links and forms point, and who is looking at it.
pub struct Place<'a> {
    pub owner: HookOwner,
    /// The section's URL, with a query already started, e.g. `/acme/app/settings?tab=webhooks`.
    pub base: &'a str,
    /// The URL the section's forms post to.
    pub action_base: &'a str,
    pub csrf_token: &'a str,
    /// Set on repository pages, whose forms name the repository they were drawn for.
    pub repository_id: Option<i64>,
}

/// Builds the section for the query the page was asked with: `hook=new` opens a blank form,
/// `hook=<id>&edit=1` opens a hook's form, `hook=<id>` lists its deliveries, and
/// `delivery=<id>` opens one delivery. `notice` names a finished action.
pub async fn build(
    pool: &DbPool,
    actor: &User,
    place: &Place<'_>,
    query: &HashMap<String, String>,
) -> Result<WebhookPanel, HookError> {
    let mut snapshot = hooks::read_settings(
        pool,
        place.owner,
        actor.id,
        query.get("hook").and_then(|id| id.parse().ok()),
        query.get("delivery").and_then(|id| id.parse().ok()),
        query
            .get("hook_after")
            .and_then(|id| id.parse().ok())
            .unwrap_or(0),
    )
    .await
    .map_err(|_| HookError::Unavailable)?
    .ok_or(HookError::Denied)?;
    let mut panel = WebhookPanel {
        base: place.base.to_owned(),
        action_base: place.action_base.to_owned(),
        csrf_token: place.csrf_token.to_owned(),
        repository_id: place.repository_id,
        notice: query
            .get("notice")
            .and_then(|notice| notice_text(notice))
            .map(str::to_owned),
        ..WebhookPanel::default()
    };
    let selected = query.get("hook").map(String::as_str);
    if selected.is_none() {
        let after = query
            .get("hook_after")
            .and_then(|id| id.parse().ok())
            .unwrap_or(0);
        let mut rows = std::mem::take(&mut snapshot.hooks);
        if rows.len() > 30 {
            rows.truncate(30);
            panel.next_href = rows
                .last()
                .map(|row| format!("{}&hook_after={}", place.base, row.id));
        }
        if after > 0 {
            panel.first_href = Some(place.base.to_owned());
        }
        panel.hooks = rows.iter().map(|hook| hook_row(place, hook)).collect();
    }
    match selected {
        Some("new") => {
            panel.editing = Some(HookDraft {
                id: None,
                payload_url: String::new(),
                content_type: JSON_CONTENT_TYPE.to_owned(),
                events: vec![events::PUSH.to_owned()],
                active: true,
                ssl_verify: true,
                has_secret: false,
            });
        }
        Some(id) => {
            let id: i64 = id.parse().map_err(|_| HookError::NotFound)?;
            let hook = snapshot
                .hook
                .as_ref()
                .filter(|hook| hook.id == id)
                .ok_or(HookError::NotFound)?;
            if query.get("edit").map(String::as_str) == Some("1") {
                panel.editing = Some(draft_of(hook));
            } else {
                panel.detail = Some(detail(place, hook, &snapshot));
            }
        }
        None => {}
    }
    Ok(panel)
}

fn notice_text(notice: &str) -> Option<&'static str> {
    Some(match notice {
        "saved" => "Webhook saved.",
        "deleted" => "Webhook deleted.",
        "pinged" => "Ping queued. Its delivery appears here in a moment.",
        "redelivered" => "Redelivery queued. It appears here in a moment.",
        _ => return None,
    })
}

fn draft_of(hook: &Webhook) -> HookDraft {
    HookDraft {
        id: Some(hook.id),
        payload_url: hook.payload_url.clone(),
        content_type: hook.content_type.clone(),
        events: if hook.events == events::EVERYTHING {
            vec![events::EVERYTHING.to_owned()]
        } else {
            hook.events.split(',').map(str::to_owned).collect()
        },
        active: hook.active,
        ssl_verify: hook.ssl_verify,
        has_secret: hook.encrypted_secret.is_some(),
    }
}

fn hook_row(place: &Place<'_>, hook: &hooks::WebhookSummary) -> HookRow {
    let last = hook.delivery_id.map(|_| hook.status_code);
    let last = match last {
        None => "No deliveries yet".to_owned(),
        Some(status_code) => match status_code {
            Some(code) => format!("Last delivery {code}"),
            None => "Last delivery failed".to_owned(),
        },
    };
    HookRow {
        id: hook.id,
        url: hook.payload_url.clone(),
        events: events_label(&hook.events),
        active: hook.active,
        last,
        open_href: format!("{}&hook={}", place.base, hook.id),
    }
}

fn events_label(stored: &str) -> String {
    if stored == events::EVERYTHING {
        "Everything".to_owned()
    } else {
        stored.replace(',', ", ")
    }
}

fn detail(place: &Place<'_>, hook: &Webhook, snapshot: &hooks::HookSettings) -> HookDetail {
    let deliveries = snapshot
        .deliveries
        .iter()
        .map(|delivery| DeliveryRow {
            id: delivery.id,
            label: label(delivery),
            status: status_text(delivery),
            ok: delivery
                .status_code
                .is_some_and(|code| (200..300).contains(&code)),
            duration: format!("{} ms", delivery.duration_ms),
            delivered: delivery
                .delivered_at
                .format("%Y-%m-%d %H:%M UTC")
                .to_string(),
            href: format!("{}&hook={}&delivery={}", place.base, hook.id, delivery.id),
        })
        .collect();
    let open = snapshot.delivery.as_ref().map(|delivery| DeliveryDetail {
        id: delivery.id,
        guid: delivery.guid.clone(),
        summary: format!("{} · {}", status_text(delivery), label(delivery)),
        status: status_text(delivery),
        error: delivery.error.clone().unwrap_or_default(),
        request_headers: delivery.request_headers.clone(),
        request_body: delivery.request_body.clone(),
        response_headers: delivery.response_headers.clone().unwrap_or_default(),
        response_body: delivery.response_body.clone().unwrap_or_default(),
    });
    HookDetail {
        id: hook.id,
        url: hook.payload_url.clone(),
        deliveries,
        open,
    }
}

fn label(delivery: &WebhookDelivery) -> String {
    match &delivery.action {
        Some(action) => format!("{} · {action}", delivery.event),
        None => delivery.event.clone(),
    }
}

fn status_text(delivery: &WebhookDelivery) -> String {
    match (delivery.status_code, &delivery.error) {
        (Some(code), _) => code.to_string(),
        (None, Some(error)) => error.clone(),
        (None, None) => "No response".to_owned(),
    }
}

/// The hook a posted settings form describes. The secret is not part of the draft: it is read
/// from the form only when saving.
pub fn draft_from_form(pairs: &[(String, String)]) -> HookDraft {
    let field = |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.trim().to_owned())
            .unwrap_or_default()
    };
    let checked = |name: &str| {
        pairs
            .iter()
            .any(|(key, value)| key == name && value == "yes")
    };
    HookDraft {
        id: field("hook_id").parse().ok(),
        payload_url: field("payload_url"),
        content_type: field("content_type"),
        events: pairs
            .iter()
            .filter(|(key, _)| key == "events")
            .map(|(_, value)| value.clone())
            .collect(),
        active: checked("active"),
        ssl_verify: checked("ssl_verify"),
        has_secret: false,
    }
}

/// Runs one posted webhook action: `webhook-save`, `webhook-delete`, `webhook-ping` or
/// `webhook-redeliver`. Returns the notice that names what was done. A refused save also returns
/// the draft, so its form can be shown again with what was typed.
pub async fn apply(
    pool: &DbPool,
    encryptor: Option<&SecretEncryptor>,
    actor: &User,
    owner: HookOwner,
    action: &str,
    form: &HashMap<String, String>,
    pairs: &[(String, String)],
) -> Result<&'static str, (HookError, Option<HookDraft>)> {
    let hook_id = form.get("hook_id").and_then(|id| id.parse::<i64>().ok());
    let refused = |error: HookError| (error, None);
    match action {
        "webhook-save" => {
            let draft = draft_from_form(pairs);
            let submitted = form_of(&draft, pairs);
            settings::save(pool, encryptor, owner, actor, draft.id, &submitted)
                .await
                .map(|_| "saved")
                .map_err(|error| (error, Some(draft)))
        }
        "webhook-delete" => {
            if form.get("confirm").map(String::as_str) != Some("yes") {
                return Err(refused(HookError::Invalid(
                    "Explicit confirmation is required.",
                )));
            }
            let id = hook_id.ok_or(HookError::NotFound).map_err(refused)?;
            settings::delete(pool, owner, actor, id)
                .await
                .map(|_| "deleted")
                .map_err(refused)
        }
        "webhook-ping" => {
            let id = hook_id.ok_or(HookError::NotFound).map_err(refused)?;
            settings::ping(pool, owner, actor, id)
                .await
                .map(|_| "pinged")
                .map_err(refused)
        }
        _ => {
            let delivery = form
                .get("delivery_id")
                .and_then(|id| id.parse::<i64>().ok());
            let (Some(hook), Some(delivery)) = (hook_id, delivery) else {
                return Err(refused(HookError::NotFound));
            };
            settings::redeliver(pool, owner, actor, hook, delivery)
                .await
                .map(|_| "redelivered")
                .map_err(refused)
        }
    }
}

/// The status and the text a refused webhook action shows.
pub fn refusal(error: &HookError) -> (axum::http::StatusCode, &'static str) {
    use axum::http::StatusCode;
    match error {
        HookError::Denied => (StatusCode::FORBIDDEN, "You cannot manage webhooks here."),
        HookError::NotFound => (StatusCode::NOT_FOUND, "Webhook not found."),
        HookError::Invalid(message) => (StatusCode::BAD_REQUEST, message),
        HookError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Webhooks are temporarily unavailable.",
        ),
    }
}

/// The save a posted form asks for. The draft's secret text is the form's `secret` field.
pub fn form_of(draft: &HookDraft, pairs: &[(String, String)]) -> HookForm {
    HookForm {
        payload_url: draft.payload_url.clone(),
        content_type: draft.content_type.clone(),
        events: draft.events.clone(),
        active: draft.active,
        ssl_verify: draft.ssl_verify,
        secret: pairs
            .iter()
            .find(|(key, _)| key == "secret")
            .map(|(_, value)| value.clone())
            .unwrap_or_default(),
    }
}
