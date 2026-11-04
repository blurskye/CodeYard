use crate::backend::db::organizations::OrganizationOutcome;
use crate::backend::db::postgres::discussions::lock_policy_authority;
use crate::backend::db::postgres::organizations::authority::authority;
use crate::backend::db::webhooks::{
    DeliveryWindow, HookOwner, NewDelivery, SealedSecret, WebhookFields, WebhookPatch,
};
use anyhow::Result;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::models::Webhook;
use codeyard_db_core::models::WebhookDelivery;
use sqlx::{PgPool, Postgres, Transaction};

const COLUMNS: &str = "id, repo_id, org_id, payload_url, content_type, encrypted_secret, secret_nonce, events, active, ssl_verify, created_by_id, created_at, updated_at, username, encrypted_password, password_nonce";
const DELIVERY_COLUMNS: &str = "id, webhook_id, guid, event, action, status_code, duration_ms, error, request_headers, request_body, response_headers, response_body, delivered_at, redelivery";

#[expect(
    clippy::too_many_arguments,
    reason = "Each argument is one filter or page bound of the settings read."
)]
pub async fn read_settings(
    pool: &PgPool,
    owner: HookOwner,
    actor_id: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    hook: Option<i64>,
    delivery: Option<i64>,
    after: i64,
    deliveries: DeliveryWindow,
) -> Result<Option<crate::backend::db::webhooks::HookSettings>> {
    let mut tx = pool.begin().await?;
    if !may_manage_hooks(&mut tx, owner, actor_id, credential, false).await? {
        return Ok(None);
    }
    let mut settings = crate::backend::db::webhooks::HookSettings {
        hooks: list_webhook_summaries(&mut *tx, owner, after).await?,
        hook: None,
        deliveries: Vec::new(),
        delivery: None,
    };
    if let Some(id) = hook {
        settings.hook = get_webhook(&mut *tx, owner, id).await?;
        if let Some(row) = &settings.hook {
            settings.deliveries = list_delivery_window(&mut *tx, row.id, &deliveries).await?;
            if let Some(id) = delivery {
                settings.delivery = get_delivery(&mut *tx, row.id, id).await?;
            }
        }
    }
    tx.commit().await?;
    Ok(Some(settings))
}

pub async fn list_managed_webhooks(
    pool: &PgPool,
    owner: HookOwner,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    offset: i64,
    limit: i64,
) -> Result<Option<Vec<Webhook>>> {
    let mut tx = pool.begin().await?;
    if !may_manage_hooks(&mut tx, owner, actor, credential, false).await? {
        return Ok(None);
    }
    let sql = format!(
        "SELECT {COLUMNS} FROM webhooks WHERE {}=$1 ORDER BY id LIMIT $2 OFFSET $3",
        owner.column()
    );
    let rows = sqlx::query_as(&sql)
        .bind(owner.id())
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(rows))
}

pub async fn list_webhooks<'e>(
    pool: impl sqlx::Executor<'e, Database = Postgres>,
    owner: HookOwner,
) -> Result<Vec<Webhook>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM webhooks WHERE {} = $1 ORDER BY id",
        owner.column()
    );
    Ok(sqlx::query_as(&sql)
        .bind(owner.id())
        .fetch_all(pool)
        .await?)
}

pub async fn list_webhook_summaries<'e>(
    pool: impl sqlx::Executor<'e, Database = Postgres>,
    owner: HookOwner,
    after: i64,
) -> Result<Vec<crate::backend::db::webhooks::WebhookSummary>> {
    let sql = format!(
        "SELECT h.id, h.payload_url, h.events, h.active, d.id AS delivery_id, d.status_code
         FROM webhooks h LEFT JOIN webhook_deliveries d ON d.id =
         (SELECT id FROM webhook_deliveries WHERE webhook_id=h.id ORDER BY id DESC LIMIT 1)
         WHERE h.{} = $1 AND h.id > $2 ORDER BY h.id LIMIT 31",
        owner.column()
    );
    Ok(sqlx::query_as(&sql)
        .bind(owner.id())
        .bind(after)
        .fetch_all(pool)
        .await?)
}

pub async fn get_webhook<'e>(
    pool: impl sqlx::Executor<'e, Database = Postgres>,
    owner: HookOwner,
    id: i64,
) -> Result<Option<Webhook>> {
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
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    write: bool,
) -> Result<bool> {
    Ok(account_manages_hooks(tx, owner, actor_id).await?
        && super::users::transitions::credential_live(tx, actor_id, credential, write).await?)
}

/// The account half of `may_manage_hooks`, for a queued send that kept only its actor.
async fn account_manages_hooks(
    tx: &mut Transaction<'_, Postgres>,
    owner: HookOwner,
    actor_id: i64,
) -> Result<bool> {
    Ok(match owner {
        // Hooks stay manageable on archived repositories, like other settings.
        HookOwner::Repository(repo_id) => {
            lock_policy_authority(tx, actor_id, repo_id, false).await?
        }
        HookOwner::Organization(org) => matches!(
            authority(tx, actor_id, org).await?,
            OrganizationOutcome::Applied(OrgRole::Owner)
        ),
    })
}

pub async fn managed_webhook(
    pool: &PgPool,
    owner: HookOwner,
    actor: i64,
    id: i64,
) -> Result<Option<Webhook>> {
    let mut tx = pool.begin().await?;
    if !account_manages_hooks(&mut tx, owner, actor).await? {
        return Ok(None);
    }
    let hook = get_webhook(&mut *tx, owner, id).await?;
    tx.commit().await?;
    Ok(hook)
}

pub async fn latest_push_for_hook(
    pool: &PgPool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    repo: i64,
    hook: i64,
    max_bytes: i64,
) -> Result<Option<String>> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        may_manage_hooks(
            &mut tx,
            HookOwner::Repository(repo),
            actor,
            credential,
            false
        )
        .await?,
        crate::backend::db::webhooks::HookAuthorityDenied
    );
    let payload: Option<String> = sqlx::query_scalar::<_, Option<String>>("SELECT CASE WHEN octet_length(payload::text)<=$3 THEN payload::text END FROM events WHERE repo_id=$1 AND kind='PushEvent' AND EXISTS(SELECT 1 FROM webhooks WHERE id=$2 AND repo_id=$1) ORDER BY id DESC LIMIT 1")
        .bind(repo).bind(hook).bind(max_bytes).fetch_optional(&mut *tx).await?.flatten();
    tx.commit().await?;
    Ok(payload)
}

/// Inserts a hook. `None` means the actor may not manage the owner's hooks.
pub async fn insert_webhook(
    pool: &PgPool,
    owner: HookOwner,
    actor_id: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    fields: &WebhookFields,
    secret: Option<&SealedSecret>,
) -> Result<Option<i64>> {
    let mut tx = pool.begin().await?;
    if !may_manage_hooks(&mut tx, owner, actor_id, credential, true).await? {
        return Ok(None);
    }
    // Serialize only hook creation for this owner; authorization's shared locks stay held.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("webhooks:{}:{}", owner.column(), owner.id()))
        .execute(&mut *tx)
        .await?;
    let hooks = list_webhooks(&mut *tx, owner).await?;
    anyhow::ensure!(
        hooks.len() < crate::backend::db::webhooks::MAX_HOOKS_PER_OWNER,
        crate::backend::db::webhooks::HookLimit
    );
    crate::backend::db::webhooks::ensure_distinct(
        &hooks,
        None,
        &fields.payload_url,
        &fields.content_type,
        fields.ssl_verify,
        &fields.events,
    )?;
    let sql = format!(
        "INSERT INTO webhooks ({}, payload_url, content_type, encrypted_secret, secret_nonce, events, active, ssl_verify, created_by_id, username, encrypted_password, password_nonce) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING id",
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
        .bind(&fields.username)
        .bind(fields.password.as_ref().map(|s| s.ciphertext.as_slice()))
        .bind(fields.password.as_ref().map(|s| s.nonce.as_slice()))
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
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    id: i64,
    fields: &WebhookFields,
    secret: Option<&SealedSecret>,
) -> Result<Option<bool>> {
    patch_webhook(
        pool,
        owner,
        actor_id,
        credential,
        id,
        &WebhookPatch::from(fields),
        secret,
    )
    .await
}

pub async fn patch_webhook(
    pool: &PgPool,
    owner: HookOwner,
    actor_id: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    id: i64,
    fields: &WebhookPatch,
    secret: Option<&SealedSecret>,
) -> Result<Option<bool>> {
    let mut tx = pool.begin().await?;
    if !may_manage_hooks(&mut tx, owner, actor_id, credential, true).await? {
        return Ok(None);
    }
    // The same lock as hook creation, so two writes cannot both pass the duplicate check.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("webhooks:{}:{}", owner.column(), owner.id()))
        .execute(&mut *tx)
        .await?;
    let current: Option<String> = sqlx::query_scalar(&format!(
        "SELECT events FROM webhooks WHERE id=$1 AND {}=$2 FOR UPDATE",
        owner.column()
    ))
    .bind(id)
    .bind(owner.id())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(current) = current else {
        return Ok(Some(false));
    };
    let events = fields.changed_events(&current)?;
    if !fields.check_distinct(
        &list_webhooks(&mut *tx, owner).await?,
        id,
        events.as_deref(),
    )? {
        return Ok(Some(false));
    }
    let sql = format!(
        "UPDATE webhooks SET payload_url = COALESCE($3,payload_url), content_type = COALESCE($4,content_type), events = COALESCE($5,events), active = COALESCE($6,active), ssl_verify = COALESCE($7,ssl_verify), \
         encrypted_secret = CASE WHEN $10 THEN NULL ELSE COALESCE($8,encrypted_secret) END, secret_nonce = CASE WHEN $10 THEN NULL ELSE COALESCE($9,secret_nonce) END, \
         username = CASE WHEN $14 THEN NULL ELSE COALESCE($11,username,CASE WHEN $12::bytea IS NOT NULL THEN '' END) END, \
         encrypted_password = CASE WHEN $14 THEN NULL ELSE COALESCE($12,encrypted_password) END, password_nonce = CASE WHEN $14 THEN NULL ELSE COALESCE($13,password_nonce) END, \
         updated_at = GREATEST(clock_timestamp(),updated_at+interval '1 microsecond') WHERE id = $1 AND {} = $2 AND ($15::timestamptz IS NULL OR updated_at=$15)",
        owner.column()
    );
    let result = sqlx::query(&sql)
        .bind(id)
        .bind(owner.id())
        .bind(&fields.payload_url)
        .bind(&fields.content_type)
        .bind(events)
        .bind(fields.active)
        .bind(fields.ssl_verify)
        .bind(secret.map(|s| s.ciphertext.as_slice()))
        .bind(secret.map(|s| s.nonce.as_slice()))
        .bind(fields.clear_secret)
        .bind(&fields.username)
        .bind(fields.password.as_ref().map(|s| s.ciphertext.as_slice()))
        .bind(fields.password.as_ref().map(|s| s.nonce.as_slice()))
        .bind(fields.clear_password)
        .bind(fields.expected_updated)
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
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    id: i64,
) -> Result<Option<bool>> {
    let mut tx = pool.begin().await?;
    if !may_manage_hooks(&mut tx, owner, actor_id, credential, true).await? {
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

// Deliveries expire by age even when their hook is idle; inserts only trim the newest 100.
pub async fn prune_expired_deliveries(pool: &PgPool) -> Result<u64> {
    let hooks = sqlx::query(
        "DELETE FROM webhook_deliveries WHERE id IN (SELECT id FROM webhook_deliveries WHERE delivered_at < clock_timestamp() - INTERVAL '30 days' ORDER BY delivered_at,id LIMIT 1000)",
    )
    .execute(pool)
    .await?
    .rows_affected();
    let apps = sqlx::query("DELETE FROM github_app_deliveries WHERE id IN (SELECT id FROM github_app_deliveries WHERE delivered_at < clock_timestamp() - INTERVAL '30 days' ORDER BY delivered_at,id LIMIT 1000)")
        .execute(pool)
        .await?
        .rows_affected();
    Ok(hooks + apps)
}
pub async fn insert_delivery(pool: &PgPool, webhook_id: i64, delivery: &NewDelivery) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO webhook_deliveries (webhook_id, guid, event, action, status_code, duration_ms, error, request_headers, request_body, response_headers, response_body, redelivery) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
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
    .bind(delivery.redelivery)
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

pub async fn list_deliveries<'e>(
    pool: impl sqlx::Executor<'e, Database = Postgres>,
    webhook_id: i64,
) -> Result<Vec<WebhookDelivery>> {
    let window = DeliveryWindow {
        bodies: true,
        ..DeliveryWindow::default()
    };
    list_delivery_window(pool, webhook_id, &window).await
}

/// The deliveries of one hook that `window` keeps, newest first.
pub async fn list_delivery_window<'e>(
    pool: impl sqlx::Executor<'e, Database = Postgres>,
    webhook_id: i64,
    window: &DeliveryWindow,
) -> Result<Vec<WebhookDelivery>> {
    let columns = if window.bodies {
        DELIVERY_COLUMNS
    } else {
        "id, webhook_id, guid, event, action, status_code, duration_ms, error, '' AS request_headers, '' AS request_body, NULL::text AS response_headers, NULL::text AS response_body, delivered_at, redelivery"
    };
    let outcome = window.outcome_sql();
    let sql = format!(
        "SELECT {columns} FROM webhook_deliveries WHERE webhook_id = $1 AND ($2::bigint IS NULL OR id < $2){outcome} ORDER BY id DESC LIMIT $3"
    );
    Ok(sqlx::query_as(&sql)
        .bind(webhook_id)
        .bind(window.before)
        .bind(window.limit.clamp(1, 101))
        .fetch_all(pool)
        .await?)
}

pub async fn latest_delivery_status<'e>(
    pool: impl sqlx::Executor<'e, Database = Postgres>,
    webhook_id: i64,
) -> Result<Option<Option<i32>>> {
    Ok(sqlx::query_scalar(
        "SELECT status_code FROM webhook_deliveries WHERE webhook_id=$1 ORDER BY id DESC LIMIT 1",
    )
    .bind(webhook_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn list_delivery_summaries<'e>(
    pool: impl sqlx::Executor<'e, Database = Postgres>,
    webhook_id: i64,
) -> Result<Vec<WebhookDelivery>> {
    list_delivery_window(pool, webhook_id, &DeliveryWindow::default()).await
}

pub async fn get_delivery<'e>(
    pool: impl sqlx::Executor<'e, Database = Postgres>,
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
