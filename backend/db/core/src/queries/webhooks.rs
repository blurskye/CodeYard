use crate::models::Webhook;
use anyhow::Result;
/// Another of the owner's hooks already sends one of the events to the same configuration.
#[derive(Debug, thiserror::Error)]
#[error("An equivalent webhook already exists")]
pub struct HookExists;

/// Refuses, with `HookExists`, a hook that would send an event another of the owner's `hooks`
/// already sends to the same URL, content type and TLS setting, as GitHub does. Sealed secrets
/// are not compared. `id` is the hook being changed, if any.
pub fn ensure_distinct(
    hooks: &[Webhook],
    id: Option<i64>,
    payload_url: &str,
    content_type: &str,
    ssl_verify: bool,
    events: &str,
) -> Result<()> {
    let duplicate = hooks.iter().any(|hook| {
        Some(hook.id) != id
            && hook.payload_url == payload_url
            && hook.content_type == content_type
            && hook.ssl_verify == ssl_verify
            && codeyard_model::webhook_events::overlap(&hook.events, events)
    });
    anyhow::ensure!(!duplicate, HookExists);
    Ok(())
}

/// Settings a webhook form saves. The secret is sealed separately by the caller.
#[derive(Debug, Clone)]
pub struct WebhookFields {
    pub username: Option<String>,
    pub password: Option<SealedSecret>,
    pub payload_url: String,
    pub content_type: String,
    pub events: String,
    pub active: bool,
    pub ssl_verify: bool,
}

#[derive(Debug, Clone, Default)]
pub struct WebhookPatch {
    pub expected_updated: Option<chrono::DateTime<chrono::Utc>>,
    pub username: Option<String>,
    pub password: Option<SealedSecret>,
    pub clear_password: bool,
    pub payload_url: Option<String>,
    pub content_type: Option<String>,
    pub events: Option<String>,
    pub add_events: Vec<String>,
    pub remove_events: Vec<String>,
    pub active: Option<bool>,
    pub ssl_verify: Option<bool>,
    pub clear_secret: bool,
}

impl From<&WebhookFields> for WebhookPatch {
    fn from(fields: &WebhookFields) -> Self {
        Self {
            expected_updated: None,
            username: fields.username.clone(),
            password: fields.password.clone(),
            clear_password: false,
            payload_url: Some(fields.payload_url.clone()),
            content_type: Some(fields.content_type.clone()),
            events: Some(fields.events.clone()),
            add_events: Vec::new(),
            remove_events: Vec::new(),
            active: Some(fields.active),
            ssl_verify: Some(fields.ssl_verify),
            clear_secret: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InvalidEvents(pub &'static str);

impl WebhookPatch {
    /// Resolve event deltas against the row locked by the webhook's write transaction.
    pub fn changed_events(&self, current: &str) -> Result<Option<String>, InvalidEvents> {
        if self.events.is_none() && self.add_events.is_empty() && self.remove_events.is_empty() {
            return Ok(None);
        }
        for delta in [&self.add_events, &self.remove_events] {
            if !delta.is_empty() {
                codeyard_model::webhook_events::subscription(delta).map_err(InvalidEvents)?;
            }
        }
        let mut selected: Vec<_> = self
            .events
            .as_deref()
            .unwrap_or(current)
            .split(',')
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        for event in &self.add_events {
            if !selected.contains(event) {
                selected.push(event.clone());
            }
        }
        selected.retain(|event| !self.remove_events.contains(event));
        codeyard_model::webhook_events::subscription(&selected)
            .map(Some)
            .map_err(InvalidEvents)
    }

    /// Refuses the patch with `HookExists` when hook `id` would then duplicate another of the
    /// owner's `hooks`. `Ok(false)` means `hooks` has no hook `id`.
    /// `events` is the subscription the patch resolves to, or `None` to keep the current one.
    pub fn check_distinct(&self, hooks: &[Webhook], id: i64, events: Option<&str>) -> Result<bool> {
        let Some(current) = hooks.iter().find(|hook| hook.id == id) else {
            return Ok(false);
        };
        ensure_distinct(
            hooks,
            Some(id),
            self.payload_url.as_deref().unwrap_or(&current.payload_url),
            self.content_type
                .as_deref()
                .unwrap_or(&current.content_type),
            self.ssl_verify.unwrap_or(current.ssl_verify),
            events.unwrap_or(&current.events),
        )?;
        Ok(true)
    }
}

#[derive(Debug, Clone)]
pub struct SealedSecret {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
}

/// One delivery attempt to record. It keeps what was sent and what came back.
#[derive(Debug, Clone)]
pub struct NewDelivery {
    pub redelivery: bool,
    pub guid: String,
    pub event: String,
    pub action: Option<String>,
    pub status_code: Option<i32>,
    pub duration_ms: i32,
    pub error: Option<String>,
    pub request_headers: String,
    pub request_body: String,
    pub response_headers: Option<String>,
    pub response_body: Option<String>,
}
