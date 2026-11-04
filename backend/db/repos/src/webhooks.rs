use anyhow::Result;
use codeyard_db_core::models::Webhook;
use codeyard_db_core::models::WebhookDelivery;
use codeyard_db_core::native_operation;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_db_core::DbPool;

/// The supported fan-out of one owner's isolated event must fit its delivery queue.
pub const MAX_HOOKS_PER_OWNER: usize = 16;

#[derive(Debug, thiserror::Error)]
#[error("The owner's webhook limit has been reached")]
pub struct HookLimit;

#[derive(Debug, thiserror::Error)]
#[error("Webhook administration is required")]
pub struct HookAuthorityDenied;

/// The repository or organization a webhook belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookOwner {
    Repository(i64),
    Organization(i64),
}

impl HookOwner {
    /// The owning column. Only these two static names ever reach SQL text.
    pub fn column(self) -> &'static str {
        match self {
            Self::Repository(_) => "repo_id",
            Self::Organization(_) => "org_id",
        }
    }

    pub fn id(self) -> i64 {
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

native_operation!(crate::postgres::webhooks::read_settings, crate::sqlite::webhooks::read_settings,
    fn read_settings(pool: &DbPool, owner: HookOwner, actor_id: i64, credential: &AccountCredential<'_>, hook: Option<i64>, delivery: Option<i64>, after: i64, deliveries: DeliveryWindow) -> Result<Option<HookSettings>>);

native_operation!(crate::postgres::webhooks::list_managed_webhooks, crate::sqlite::webhooks::list_managed_webhooks,
    fn list_managed_webhooks(pool: &DbPool, owner: HookOwner, actor: i64, credential: &AccountCredential<'_>, offset: i64, limit: i64) -> Result<Option<Vec<Webhook>>>);
native_operation!(crate::postgres::webhooks::patch_webhook, crate::sqlite::webhooks::patch_webhook,
    fn patch_webhook(pool: &DbPool, owner: HookOwner, actor: i64, credential: &AccountCredential<'_>, id: i64, patch: &WebhookPatch, secret: Option<&SealedSecret>) -> Result<Option<bool>>);

native_operation!(crate::postgres::webhooks::list_webhooks, crate::sqlite::webhooks::list_webhooks,
    fn list_webhooks(pool: &DbPool, owner: HookOwner) -> Result<Vec<Webhook>>);

native_operation!(crate::postgres::webhooks::managed_webhook, crate::sqlite::webhooks::managed_webhook,
    fn managed_webhook(pool: &DbPool, owner: HookOwner, actor: i64, id: i64) -> Result<Option<Webhook>>);
native_operation!(crate::postgres::webhooks::latest_push_for_hook, crate::sqlite::webhooks::latest_push_for_hook,
    fn latest_push_for_hook(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, repo: i64, hook: i64, max_bytes: i64) -> Result<Option<String>>);

native_operation!(crate::postgres::webhooks::get_webhook, crate::sqlite::webhooks::get_webhook,
    fn get_webhook(pool: &DbPool, owner: HookOwner, id: i64) -> Result<Option<Webhook>>);

// The writes below return `None` when `actor_id` may not manage the owner's hooks.
native_operation!(crate::postgres::webhooks::insert_webhook, crate::sqlite::webhooks::insert_webhook,
    fn insert_webhook(pool: &DbPool, owner: HookOwner, actor_id: i64, credential: &AccountCredential<'_>, fields: &WebhookFields, secret: Option<&SealedSecret>) -> Result<Option<i64>>);

// A `None` secret keeps the stored one. `Some(false)` means the webhook is not on `owner`.
native_operation!(crate::postgres::webhooks::update_webhook, crate::sqlite::webhooks::update_webhook,
    fn update_webhook(pool: &DbPool, owner: HookOwner, actor_id: i64, credential: &AccountCredential<'_>, id: i64, fields: &WebhookFields, secret: Option<&SealedSecret>) -> Result<Option<bool>>);

// `Some(false)` means the webhook is not on `owner`.
native_operation!(crate::postgres::webhooks::delete_webhook, crate::sqlite::webhooks::delete_webhook,
    fn delete_webhook(pool: &DbPool, owner: HookOwner, actor_id: i64, credential: &AccountCredential<'_>, id: i64) -> Result<Option<bool>>);

native_operation!(crate::postgres::webhooks::prune_expired_deliveries, crate::sqlite::webhooks::prune_expired_deliveries,
    fn prune_expired_deliveries(pool: &DbPool) -> Result<u64>);

// Records a delivery and drops the hook's deliveries past 30 days or beyond the newest 100.
native_operation!(crate::postgres::webhooks::insert_delivery, crate::sqlite::webhooks::insert_delivery,
    fn insert_delivery(pool: &DbPool, webhook_id: i64, delivery: &NewDelivery) -> Result<()>);

// Newest first, at most 100.
native_operation!(crate::postgres::webhooks::list_deliveries, crate::sqlite::webhooks::list_deliveries,
    fn list_deliveries(pool: &DbPool, webhook_id: i64) -> Result<Vec<WebhookDelivery>>);

native_operation!(crate::postgres::webhooks::latest_delivery_status, crate::sqlite::webhooks::latest_delivery_status,
    fn latest_delivery_status(pool: &DbPool, webhook_id: i64) -> Result<Option<Option<i32>>>);

native_operation!(crate::postgres::webhooks::list_delivery_summaries, crate::sqlite::webhooks::list_delivery_summaries,
    fn list_delivery_summaries(pool: &DbPool, webhook_id: i64) -> Result<Vec<WebhookDelivery>>);

native_operation!(crate::postgres::webhooks::get_delivery, crate::sqlite::webhooks::get_delivery,
    fn get_delivery(pool: &DbPool, webhook_id: i64, delivery_id: i64) -> Result<Option<WebhookDelivery>>);

use codeyard_db_core::queries::webhooks::WebhookFields;

use codeyard_db_core::queries::webhooks::NewDelivery;

use codeyard_db_core::queries::webhooks::WebhookPatch;

use codeyard_db_core::queries::webhooks::SealedSecret;

use codeyard_db_core::queries::webhooks::NewDelivery;

use codeyard_db_core::queries::webhooks::SealedSecret;

use codeyard_db_core::queries::webhooks::WebhookFields;

use codeyard_db_core::queries::webhooks::WebhookPatch;
