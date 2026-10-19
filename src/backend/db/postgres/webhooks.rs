use crate::backend::db::models::{OrgRole, Webhook, WebhookDelivery};
use crate::backend::db::organizations::OrganizationOutcome;
use crate::backend::db::postgres::discussions::lock_admin_authority;
use crate::backend::db::postgres::organizations::authority::authority;
use crate::backend::db::webhooks::{HookOwner, NewDelivery, SealedSecret, WebhookFields};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

const COLUMNS: &str = "id, repo_id, org_id, payload_url, content_type, encrypted_secret, secret_nonce, events, active, ssl_verify, created_by_id, created_at, updated_at";
const DELIVERY_COLUMNS: &str = "id, webhook_id, guid, event, action, status_code, duration_ms, error, request_headers, request_body, response_headers, response_body, delivered_at";

pub async fn list_webhooks(pool: &PgPool, owner: HookOwner) -> Result<Vec<Webhook>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM webhooks WHERE {} = $1 ORDER BY id",
        owner.column()
    );
    Ok(sqlx::query_as(&sql)
        .bind(owner.id())
        .fetch_all(pool)
        .await?)
}

pub async fn get_webhook(pool: &PgPool, owner: HookOwner, id: i64) -> Result<Option<Webhook>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM webhooks WHERE id = $1 AND {} = $2",
        owner.column()
    );
    Ok(sqlx::query_as(&sql)
        .bind(id)
        .bind(owner.id())
        .fetch_optional(pool)
        .await?)
}

/// Whether `actor_id` may manage the hooks of `owner`, checked in `tx`: a repository's
/// administrators, or an organization's owners. The locks it takes last until `tx` ends, so a
/// membership change cannot commit between this check and the write that follows it.
async fn may_manage_hooks(
    tx: &mut Transaction<'_, Postgres>,
    owner: HookOwner,
    actor_id: i64,
) -> Result<bool> {
    match owner {
        HookOwner::Repository(repo_id) => lock_admin_authority(tx, actor_id, repo_id).await,
        HookOwner::Organization(org) => Ok(matches!(
            authority(tx, actor_id, org).await?,
            OrganizationOutcome::Applied(OrgRole::Owner)
        )),
    }
}

/// Inserts a hook. `None` means the actor may not manage the owner's hooks.
pub async fn insert_webhook(
    pool: &PgPool,
    owner: HookOwner,
    actor_id: i64,
    fields: &WebhookFields,
    secret: Option<&SealedSecret>,
) -> Result<Option<i64>> {
    let mut tx = pool.begin().await?;
    if !may_manage_hooks(&mut tx, owner, actor_id).await? {
        return Ok(None);
    }
    let sql = format!(
        "INSERT INTO webhooks ({}, payload_url, content_type, encrypted_secret, secret_nonce, events, active, ssl_verify, created_by_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
        owner.column()
    );
    let id = sqlx::query_scalar(&sql)
        .bind(owner.id())
        .bind(&fields.payload_url)
        .bind(&fields.content_type)
        .bind(secret.map(|s| s.ciphertext.as_slice()))
        .bind(secret.map(|s| s.nonce.as_slice()))
        .bind(&fields.events)
        .bind(fields.active)
        .bind(fields.ssl_verify)
        .bind(actor_id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(id))
}

/// Updates a hook. `None` means the actor may not manage the owner's hooks; `Some(false)` means
/// the owner has no such hook.
pub async fn update_webhook(
    pool: &PgPool,
    owner: HookOwner,
    actor_id: i64,
    id: i64,
    fields: &WebhookFields,
    secret: Option<&SealedSecret>,
) -> Result<Option<bool>> {
    let mut tx = pool.begin().await?;
    if !may_manage_hooks(&mut tx, owner, actor_id).await? {
        return Ok(None);
    }
    let sql = format!(
        "UPDATE webhooks SET payload_url = $3, content_type = $4, events = $5, active = $6, ssl_verify = $7, \
         encrypted_secret = COALESCE($8, encrypted_secret), secret_nonce = COALESCE($9, secret_nonce), \
         updated_at = NOW() WHERE id = $1 AND {} = $2",
        owner.column()
    );
    let result = sqlx::query(&sql)
        .bind(id)
        .bind(owner.id())
        .bind(&fields.payload_url)
        .bind(&fields.content_type)
        .bind(&fields.events)
        .bind(fields.active)
        .bind(fields.ssl_verify)
        .bind(secret.map(|s| s.ciphertext.as_slice()))
        .bind(secret.map(|s| s.nonce.as_slice()))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(result.rows_affected() > 0))
}

/// Deletes a hook. `None` means the actor may not manage the owner's hooks; `Some(false)` means
/// the owner has no such hook.
pub async fn delete_webhook(
    pool: &PgPool,
    owner: HookOwner,
    actor_id: i64,
    id: i64,
) -> Result<Option<bool>> {
    let mut tx = pool.begin().await?;
    if !may_manage_hooks(&mut tx, owner, actor_id).await? {
        return Ok(None);
    }
    let sql = format!(
        "DELETE FROM webhooks WHERE id = $1 AND {} = $2",
        owner.column()
    );
    let result = sqlx::query(&sql)
        .bind(id)
        .bind(owner.id())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(result.rows_affected() > 0))
}

pub async fn insert_delivery(pool: &PgPool, webhook_id: i64, delivery: &NewDelivery) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO webhook_deliveries (webhook_id, guid, event, action, status_code, duration_ms, error, request_headers, request_body, response_headers, response_body) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(webhook_id)
    .bind(&delivery.guid)
    .bind(&delivery.event)
    .bind(&delivery.action)
    .bind(delivery.status_code)
    .bind(delivery.duration_ms)
    .bind(&delivery.error)
    .bind(&delivery.request_headers)
    .bind(&delivery.request_body)
    .bind(&delivery.response_headers)
    .bind(&delivery.response_body)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM webhook_deliveries WHERE webhook_id = $1 AND (delivered_at < NOW() - INTERVAL '30 days' \
         OR id NOT IN (SELECT id FROM webhook_deliveries WHERE webhook_id = $1 ORDER BY id DESC LIMIT 100))",
    )
    .bind(webhook_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn list_deliveries(pool: &PgPool, webhook_id: i64) -> Result<Vec<WebhookDelivery>> {
    let sql = format!(
        "SELECT {DELIVERY_COLUMNS} FROM webhook_deliveries WHERE webhook_id = $1 ORDER BY id DESC LIMIT 100"
    );
    Ok(sqlx::query_as(&sql)
        .bind(webhook_id)
        .fetch_all(pool)
        .await?)
}

pub async fn get_delivery(
    pool: &PgPool,
    webhook_id: i64,
    delivery_id: i64,
) -> Result<Option<WebhookDelivery>> {
    let sql = format!(
        "SELECT {DELIVERY_COLUMNS} FROM webhook_deliveries WHERE id = $1 AND webhook_id = $2"
    );
    Ok(sqlx::query_as(&sql)
        .bind(delivery_id)
        .bind(webhook_id)
        .fetch_optional(pool)
        .await?)
}
