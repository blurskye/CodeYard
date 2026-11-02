use crate::backend::db::{
    github_app_hooks::{Delivery, Subscription},
    models::Webhook,
    webhooks::{NewDelivery, SealedSecret, WebhookFields},
};
use anyhow::Result;
use sqlx::PgPool;
const HOOK:&str="NULL AS username,NULL AS encrypted_password,NULL AS password_nonce,h.app_id AS id,h.app_id,NULL AS repo_id,NULL AS org_id,h.payload_url,h.content_type,h.encrypted_secret,h.secret_nonce,h.events,h.active,h.ssl_verify,a.owner_id AS created_by_id,h.created_at,h.updated_at";
pub async fn hook(pool: &PgPool, app: i64) -> Result<Option<Webhook>> {
    Ok(sqlx::query_as(&format!("SELECT {HOOK} FROM github_app_hooks h JOIN github_apps a ON a.id=h.app_id WHERE h.app_id=$1")).bind(app).fetch_optional(pool).await?)
}
pub async fn save(
    pool: &PgPool,
    app: i64,
    fields: &WebhookFields,
    secret: Option<&SealedSecret>,
    clear: bool,
) -> Result<()> {
    let mut connection = pool.acquire().await?;
    save_in_connection(&mut connection, app, fields, secret, clear).await
}
pub(super) async fn save_in_connection(
    pool: &mut sqlx::PgConnection,
    app: i64,
    fields: &WebhookFields,
    secret: Option<&SealedSecret>,
    clear: bool,
) -> Result<()> {
    sqlx::query("INSERT INTO github_app_hooks(app_id,payload_url,content_type,encrypted_secret,secret_nonce,events,active,ssl_verify) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(app_id) DO UPDATE SET payload_url=excluded.payload_url,content_type=excluded.content_type,events=excluded.events,active=excluded.active,ssl_verify=excluded.ssl_verify,encrypted_secret=CASE WHEN $9 THEN NULL ELSE COALESCE(excluded.encrypted_secret,github_app_hooks.encrypted_secret) END,secret_nonce=CASE WHEN $9 THEN NULL ELSE COALESCE(excluded.secret_nonce,github_app_hooks.secret_nonce) END,updated_at=clock_timestamp()").bind(app).bind(&fields.payload_url).bind(&fields.content_type).bind(secret.map(|s|s.ciphertext.as_slice())).bind(secret.map(|s|s.nonce.as_slice())).bind(&fields.events).bind(fields.active).bind(fields.ssl_verify).bind(clear).execute(pool).await?;
    Ok(())
}
pub async fn for_repository(pool: &PgPool, repo: i64) -> Result<Vec<Subscription>> {
    Ok(sqlx::query_as(&format!("SELECT {HOOK},i.id AS installation_id,a.permissions FROM github_app_hooks h JOIN github_apps a ON a.id=h.app_id JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id JOIN github_app_installations i ON i.app_id=a.id JOIN repositories r ON r.id=$1 WHERE h.active AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)) AND (r.owner_org_id IS NULL OR EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=r.owner_org_id AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password)) ORDER BY h.app_id")).bind(repo).fetch_all(pool).await?)
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
        "SELECT id,webhook_id,guid,event,action,status_code,duration_ms,error,'' AS request_headers,'' AS request_body,NULL AS response_headers,NULL AS response_body,redelivery,installation_id,repository_id,url,delivered_at FROM github_app_deliveries WHERE webhook_id=$1 ORDER BY id DESC LIMIT 100",
    )
    .bind(app)
    .fetch_all(pool)
    .await?)
}
pub async fn delivery(pool: &PgPool, app: i64, id: i64) -> Result<Option<Delivery>> {
    Ok(
        sqlx::query_as("SELECT * FROM github_app_deliveries WHERE webhook_id=$1 AND id=$2")
            .bind(app)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
