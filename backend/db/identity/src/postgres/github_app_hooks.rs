use crate::github_app_hooks::Delivery;
use crate::github_app_hooks::Subscription;
use anyhow::Result;
use codeyard_database::models::Webhook;
use codeyard_database::pool::PgPool;
use codeyard_database::queries::webhooks::NewDelivery;
use codeyard_database::queries::webhooks::SealedSecret;
use codeyard_database::queries::webhooks::WebhookPatch;

const HOOK:&str="NULL AS username,NULL AS encrypted_password,NULL AS password_nonce,h.app_id AS id,h.app_id,NULL AS repo_id,NULL AS org_id,h.payload_url,h.content_type,h.encrypted_secret,h.secret_nonce,h.events,h.active,h.ssl_verify,a.owner_id AS created_by_id,h.created_at,h.updated_at";
pub async fn hook(pool: &PgPool, app: i64) -> Result<Option<Webhook>> {
    Ok(sqlx::query_as(&format!("SELECT {HOOK} FROM github_app_hooks h JOIN github_apps a ON a.id=h.app_id WHERE h.app_id=$1")).bind(app).fetch_optional(pool).await?)
}
pub async fn save(
    pool: &PgPool,
    app: i64,
    patch: &WebhookPatch,
    secret: Option<&SealedSecret>,
    clear: bool,
) -> Result<bool> {
    let mut connection = pool.acquire().await?;
    save_in_connection(&mut connection, app, patch, secret, clear).await
}
pub(super) async fn save_in_connection(
    connection: &mut sqlx::PgConnection,
    app: i64,
    patch: &WebhookPatch,
    secret: Option<&SealedSecret>,
    clear: bool,
) -> Result<bool> {
    let ciphertext = secret.map(|secret| secret.ciphertext.as_slice());
    let nonce = secret.map(|secret| secret.nonce.as_slice());
    if sqlx::query("UPDATE github_app_hooks SET payload_url=COALESCE($2,payload_url),content_type=COALESCE($3,content_type),events=COALESCE($4,events),active=COALESCE($5,active),ssl_verify=COALESCE($6,ssl_verify),encrypted_secret=CASE WHEN $9 THEN NULL ELSE COALESCE($7,encrypted_secret) END,secret_nonce=CASE WHEN $9 THEN NULL ELSE COALESCE($8,secret_nonce) END,updated_at=clock_timestamp() WHERE app_id=$1")
        .bind(app).bind(&patch.payload_url).bind(&patch.content_type).bind(&patch.events).bind(patch.active).bind(patch.ssl_verify).bind(ciphertext).bind(nonce).bind(clear)
        .execute(&mut *connection).await?.rows_affected() > 0
    {
        return Ok(true);
    }
    let (Some(url), Some(content_type), Some(events), Some(active), Some(ssl_verify)) = (
        &patch.payload_url,
        &patch.content_type,
        &patch.events,
        patch.active,
        patch.ssl_verify,
    ) else {
        return Ok(false);
    };
    // A first save names every field, so one that races another first save replaces it whole.
    sqlx::query("INSERT INTO github_app_hooks(app_id,payload_url,content_type,encrypted_secret,secret_nonce,events,active,ssl_verify) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(app_id) DO UPDATE SET payload_url=excluded.payload_url,content_type=excluded.content_type,events=excluded.events,active=excluded.active,ssl_verify=excluded.ssl_verify,encrypted_secret=CASE WHEN $9 THEN NULL ELSE COALESCE(excluded.encrypted_secret,github_app_hooks.encrypted_secret) END,secret_nonce=CASE WHEN $9 THEN NULL ELSE COALESCE(excluded.secret_nonce,github_app_hooks.secret_nonce) END,updated_at=clock_timestamp()")
        .bind(app).bind(url).bind(content_type).bind(ciphertext).bind(nonce).bind(events).bind(active).bind(ssl_verify).bind(clear)
        .execute(connection).await?;
    Ok(true)
}
pub async fn for_repository(pool: &PgPool, repo: i64) -> Result<Vec<Subscription>> {
    subscriptions(pool, repo, None, None, true).await
}
pub async fn subscription(
    pool: &PgPool,
    repo: i64,
    app: i64,
    installation: i64,
    automatic: bool,
) -> Result<Option<Subscription>> {
    Ok(
        subscriptions(pool, repo, Some(app), Some(installation), automatic)
            .await?
            .pop(),
    )
}
pub async fn for_repository_app(pool: &PgPool, repo: i64, app: i64) -> Result<Vec<Subscription>> {
    subscriptions(pool, repo, Some(app), None, true).await
}
async fn subscriptions(
    pool: &PgPool,
    repo: i64,
    app: Option<i64>,
    installation: Option<i64>,
    automatic: bool,
) -> Result<Vec<Subscription>> {
    Ok(sqlx::query_as(&format!("SELECT {HOOK},i.id AS installation_id,a.permissions FROM github_app_hooks h JOIN github_apps a ON a.id=h.app_id JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id JOIN github_app_installations i ON i.app_id=a.id JOIN repositories r ON r.id=$1 WHERE (NOT $4 OR h.active) AND ($2::bigint IS NULL OR h.app_id=$2) AND ($3::bigint IS NULL OR i.id=$3) AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password AND NOT EXISTS(SELECT 1 FROM organization_blocks blocked WHERE blocked.org_id=r.owner_org_id AND blocked.user_id=a.bot_id) AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)) AND (r.owner_org_id IS NULL OR EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=r.owner_org_id AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password)) ORDER BY h.app_id")).bind(repo).bind(app).bind(installation).bind(automatic).fetch_all(pool).await?)
}
pub async fn insert_delivery(
    pool: &PgPool,
    app: i64,
    record: &NewDelivery,
    redelivery: bool,
    installation: Option<i64>,
    repo: Option<i64>,
    url: &str,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO github_app_deliveries(webhook_id,guid,event,action,status_code,duration_ms,error,request_headers,request_body,response_headers,response_body,redelivery,installation_id,repository_id,url) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)").bind(app).bind(&record.guid).bind(&record.event).bind(&record.action).bind(record.status_code).bind(record.duration_ms).bind(&record.error).bind(&record.request_headers).bind(&record.request_body).bind(&record.response_headers).bind(&record.response_body).bind(redelivery).bind(installation).bind(repo).bind(url).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM github_app_deliveries WHERE webhook_id=$1 AND (delivered_at<clock_timestamp()-INTERVAL '30 days' OR id NOT IN(SELECT id FROM github_app_deliveries WHERE webhook_id=$1 ORDER BY id DESC LIMIT 100))").bind(app).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn deliveries(pool: &PgPool, app: i64) -> Result<Vec<Delivery>> {
    Ok(sqlx::query_as(
        "SELECT id,webhook_id,guid,event,action,status_code,duration_ms,error,'' AS request_headers,'' AS request_body,NULL AS response_headers,NULL AS response_body,redelivery,installation_id,repository_id,url,delivered_at FROM github_app_deliveries d WHERE webhook_id=$1 AND EXISTS(SELECT 1 FROM github_apps a JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id WHERE a.id=d.webhook_id AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password) ORDER BY id DESC LIMIT 100",
    )
    .bind(app)
    .fetch_all(pool)
    .await?)
}
pub async fn delivery(pool: &PgPool, app: i64, id: i64) -> Result<Option<Delivery>> {
    Ok(
        sqlx::query_as("SELECT * FROM github_app_deliveries d WHERE webhook_id=$1 AND id=$2 AND EXISTS(SELECT 1 FROM github_apps a JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id WHERE a.id=d.webhook_id AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password)")
            .bind(app)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
