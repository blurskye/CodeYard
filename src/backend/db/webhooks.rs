//! Webhook rows and their delivery records. Each change checks, in its own transaction, that
//! the actor may manage the owner's hooks. Reads do no checks; callers decide what they show.
use crate::backend::db::models::{Webhook, WebhookDelivery};
use crate::backend::db::DbPool;
use anyhow::Result;

/// The repository or organization a webhook belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Settings a webhook form saves. The secret is sealed separately by the caller.
#[derive(Debug, Clone)]
pub struct WebhookFields {
    pub payload_url: String,
    pub content_type: String,
    pub events: String,
    pub active: bool,
    pub ssl_verify: bool,
}

#[derive(Debug, Clone)]
pub struct SealedSecret {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
}

/// One delivery attempt to record. It keeps what was sent and what came back.
#[derive(Debug, Clone)]
pub struct NewDelivery {
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

native_operation!(crate::backend::db::postgres::webhooks::list_webhooks, crate::backend::db::sqlite::webhooks::list_webhooks,
    fn list_webhooks(pool: &DbPool, owner: HookOwner) -> Result<Vec<Webhook>>);

native_operation!(crate::backend::db::postgres::webhooks::get_webhook, crate::backend::db::sqlite::webhooks::get_webhook,
    fn get_webhook(pool: &DbPool, owner: HookOwner, id: i64) -> Result<Option<Webhook>>);

// The writes below return `None` when `actor_id` may not manage the owner's hooks.
native_operation!(crate::backend::db::postgres::webhooks::insert_webhook, crate::backend::db::sqlite::webhooks::insert_webhook,
    fn insert_webhook(pool: &DbPool, owner: HookOwner, actor_id: i64, fields: &WebhookFields, secret: Option<&SealedSecret>) -> Result<Option<i64>>);

// A `None` secret keeps the stored one. `Some(false)` means the webhook is not on `owner`.
native_operation!(crate::backend::db::postgres::webhooks::update_webhook, crate::backend::db::sqlite::webhooks::update_webhook,
    fn update_webhook(pool: &DbPool, owner: HookOwner, actor_id: i64, id: i64, fields: &WebhookFields, secret: Option<&SealedSecret>) -> Result<Option<bool>>);

// `Some(false)` means the webhook is not on `owner`.
native_operation!(crate::backend::db::postgres::webhooks::delete_webhook, crate::backend::db::sqlite::webhooks::delete_webhook,
    fn delete_webhook(pool: &DbPool, owner: HookOwner, actor_id: i64, id: i64) -> Result<Option<bool>>);

// Records a delivery and drops the hook's deliveries past 30 days or beyond the newest 100.
native_operation!(crate::backend::db::postgres::webhooks::insert_delivery, crate::backend::db::sqlite::webhooks::insert_delivery,
    fn insert_delivery(pool: &DbPool, webhook_id: i64, delivery: &NewDelivery) -> Result<()>);

// Newest first, at most 100.
native_operation!(crate::backend::db::postgres::webhooks::list_deliveries, crate::backend::db::sqlite::webhooks::list_deliveries,
    fn list_deliveries(pool: &DbPool, webhook_id: i64) -> Result<Vec<WebhookDelivery>>);

native_operation!(crate::backend::db::postgres::webhooks::get_delivery, crate::backend::db::sqlite::webhooks::get_delivery,
    fn get_delivery(pool: &DbPool, webhook_id: i64, delivery_id: i64) -> Result<Option<WebhookDelivery>>);
