//! Webhook rows and their delivery records. Each change checks, in its own transaction, that
//! the actor may manage the owner's hooks. Settings reads retain that authority too.
use crate::backend::db::users::accounts::AccountCredential;
use anyhow::Result;
use codeyard_db_core::models::Webhook;
use codeyard_db_core::models::WebhookDelivery;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

/// The supported fan-out of one owner's isolated event must fit its delivery queue.
pub const MAX_HOOKS_PER_OWNER: usize = 16;

#[derive(Debug, thiserror::Error)]
#[error("The owner's webhook limit has been reached")]
pub struct HookLimit;

#[derive(Debug, thiserror::Error)]
#[error("Webhook administration is required")]
pub struct HookAuthorityDenied;

/// Another of the owner's hooks already sends one of the events to the same configuration.
#[derive(Debug, thiserror::Error)]
#[error("An equivalent webhook already exists")]
pub struct HookExists;

/// Refuses, with `HookExists`, a hook that would send an event another of the owner's `hooks`
/// already sends to the same URL, content type and TLS setting, as GitHub does. Sealed secrets
/// are not compared. `id` is the hook being changed, if any.
pub(crate) fn ensure_distinct(
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
            && crate::backend::webhooks::events::overlap(&hook.events, events)
    });
    anyhow::ensure!(!duplicate, HookExists);
    Ok(())
}

/// The repository or organization a webhook belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookOwner {
    Repository(i64),
    Organization(i64),
}

impl HookOwner {
    /// The owning column. Only these two static names ever reach SQL text.
    pub(crate) fn column(self) -> &'static str {
        match self {
            Self::Repository(_) => "repo_id",
            Self::Organization(_) => "org_id",
        }
    }

    pub(crate) fn id(self) -> i64 {
        match self {
            Self::Repository(id) | Self::Organization(id) => id,
        }
    }
}

/// One settings-list row, without secrets or delivery bodies.
#[derive(Debug, sqlx::FromRow)]
pub struct WebhookSummary {
    pub id: i64,
    pub payload_url: String,
    pub events: String,
    pub active: bool,
    pub delivery_id: Option<i64>,
    pub status_code: Option<i32>,
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
    pub(crate) fn changed_events(&self, current: &str) -> Result<Option<String>, InvalidEvents> {
        use crate::backend::webhooks::events;
        if self.events.is_none() && self.add_events.is_empty() && self.remove_events.is_empty() {
            return Ok(None);
        }
        for delta in [&self.add_events, &self.remove_events] {
            if !delta.is_empty() {
                events::subscription(delta).map_err(InvalidEvents)?;
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
        events::subscription(&selected)
            .map(Some)
            .map_err(InvalidEvents)
    }

    /// Refuses the patch with `HookExists` when hook `id` would then duplicate another of the
    /// owner's `hooks`. `Ok(false)` means `hooks` has no hook `id`.
    /// `events` is the subscription the patch resolves to, or `None` to keep the current one.
    pub(crate) fn check_distinct(
        &self,
        hooks: &[Webhook],
        id: i64,
        events: Option<&str>,
    ) -> Result<bool> {
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

/// Which deliveries by status code a history keeps: successes are 2xx, as in
/// `WebhookDelivery::succeeded`; everything else, including deliveries without a response, failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Success,
    Failure,
}

/// The delivery history a settings read loads, newest first. Without `bodies` the request and
/// response bodies stay empty.
#[derive(Clone, Copy, Debug)]
pub struct DeliveryWindow {
    pub bodies: bool,
    /// Only deliveries with an id below this one.
    pub before: Option<i64>,
    pub outcome: Option<DeliveryOutcome>,
    /// At most this many rows, up to 101: a page of 100 and one more to tell that another follows.
    pub limit: i64,
}

impl Default for DeliveryWindow {
    fn default() -> Self {
        Self {
            bodies: false,
            before: None,
            outcome: None,
            limit: 100,
        }
    }
}

impl DeliveryWindow {
    /// The SQL condition on the `status_code` column that keeps only the wanted outcome.
    pub(crate) fn outcome_sql(&self) -> &'static str {
        match self.outcome {
            None => "",
            Some(DeliveryOutcome::Success) => " AND status_code BETWEEN 200 AND 299",
            Some(DeliveryOutcome::Failure) => {
                " AND (status_code IS NULL OR status_code NOT BETWEEN 200 AND 299)"
            }
        }
    }
}

/// The settings projection, loaded under the same authority as hook changes.
pub struct HookSettings {
    pub hooks: Vec<WebhookSummary>,
    pub hook: Option<Webhook>,
    pub deliveries: Vec<WebhookDelivery>,
    pub delivery: Option<WebhookDelivery>,
}

native_operation!(crate::backend::db::postgres::webhooks::read_settings, crate::backend::db::sqlite::webhooks::read_settings,
    fn read_settings(pool: &DbPool, owner: HookOwner, actor_id: i64, credential: &AccountCredential<'_>, hook: Option<i64>, delivery: Option<i64>, after: i64, deliveries: DeliveryWindow) -> Result<Option<HookSettings>>);

native_operation!(crate::backend::db::postgres::webhooks::list_managed_webhooks, crate::backend::db::sqlite::webhooks::list_managed_webhooks,
    fn list_managed_webhooks(pool: &DbPool, owner: HookOwner, actor: i64, credential: &AccountCredential<'_>, offset: i64, limit: i64) -> Result<Option<Vec<Webhook>>>);
native_operation!(crate::backend::db::postgres::webhooks::patch_webhook, crate::backend::db::sqlite::webhooks::patch_webhook,
    fn patch_webhook(pool: &DbPool, owner: HookOwner, actor: i64, credential: &AccountCredential<'_>, id: i64, patch: &WebhookPatch, secret: Option<&SealedSecret>) -> Result<Option<bool>>);

native_operation!(crate::backend::db::postgres::webhooks::list_webhooks, crate::backend::db::sqlite::webhooks::list_webhooks,
    fn list_webhooks(pool: &DbPool, owner: HookOwner) -> Result<Vec<Webhook>>);

native_operation!(crate::backend::db::postgres::webhooks::managed_webhook, crate::backend::db::sqlite::webhooks::managed_webhook,
    fn managed_webhook(pool: &DbPool, owner: HookOwner, actor: i64, id: i64) -> Result<Option<Webhook>>);
native_operation!(crate::backend::db::postgres::webhooks::latest_push_for_hook, crate::backend::db::sqlite::webhooks::latest_push_for_hook,
    fn latest_push_for_hook(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, repo: i64, hook: i64, max_bytes: i64) -> Result<Option<String>>);

native_operation!(crate::backend::db::postgres::webhooks::get_webhook, crate::backend::db::sqlite::webhooks::get_webhook,
    fn get_webhook(pool: &DbPool, owner: HookOwner, id: i64) -> Result<Option<Webhook>>);

// The writes below return `None` when `actor_id` may not manage the owner's hooks.
native_operation!(crate::backend::db::postgres::webhooks::insert_webhook, crate::backend::db::sqlite::webhooks::insert_webhook,
    fn insert_webhook(pool: &DbPool, owner: HookOwner, actor_id: i64, credential: &AccountCredential<'_>, fields: &WebhookFields, secret: Option<&SealedSecret>) -> Result<Option<i64>>);

// A `None` secret keeps the stored one. `Some(false)` means the webhook is not on `owner`.
native_operation!(crate::backend::db::postgres::webhooks::update_webhook, crate::backend::db::sqlite::webhooks::update_webhook,
    fn update_webhook(pool: &DbPool, owner: HookOwner, actor_id: i64, credential: &AccountCredential<'_>, id: i64, fields: &WebhookFields, secret: Option<&SealedSecret>) -> Result<Option<bool>>);

// `Some(false)` means the webhook is not on `owner`.
native_operation!(crate::backend::db::postgres::webhooks::delete_webhook, crate::backend::db::sqlite::webhooks::delete_webhook,
    fn delete_webhook(pool: &DbPool, owner: HookOwner, actor_id: i64, credential: &AccountCredential<'_>, id: i64) -> Result<Option<bool>>);

native_operation!(crate::backend::db::postgres::webhooks::prune_expired_deliveries, crate::backend::db::sqlite::webhooks::prune_expired_deliveries,
    fn prune_expired_deliveries(pool: &DbPool) -> Result<u64>);

// Records a delivery and drops the hook's deliveries past 30 days or beyond the newest 100.
native_operation!(crate::backend::db::postgres::webhooks::insert_delivery, crate::backend::db::sqlite::webhooks::insert_delivery,
    fn insert_delivery(pool: &DbPool, webhook_id: i64, delivery: &NewDelivery) -> Result<()>);

// Newest first, at most 100.
native_operation!(crate::backend::db::postgres::webhooks::list_deliveries, crate::backend::db::sqlite::webhooks::list_deliveries,
    fn list_deliveries(pool: &DbPool, webhook_id: i64) -> Result<Vec<WebhookDelivery>>);

native_operation!(crate::backend::db::postgres::webhooks::latest_delivery_status, crate::backend::db::sqlite::webhooks::latest_delivery_status,
    fn latest_delivery_status(pool: &DbPool, webhook_id: i64) -> Result<Option<Option<i32>>>);

native_operation!(crate::backend::db::postgres::webhooks::list_delivery_summaries, crate::backend::db::sqlite::webhooks::list_delivery_summaries,
    fn list_delivery_summaries(pool: &DbPool, webhook_id: i64) -> Result<Vec<WebhookDelivery>>);

native_operation!(crate::backend::db::postgres::webhooks::get_delivery, crate::backend::db::sqlite::webhooks::get_delivery,
    fn get_delivery(pool: &DbPool, webhook_id: i64, delivery_id: i64) -> Result<Option<WebhookDelivery>>);
