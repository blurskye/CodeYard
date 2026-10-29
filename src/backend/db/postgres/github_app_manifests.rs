use crate::backend::db::{
    github_app_manifests::Draft,
    github_apps::App,
    webhooks::{SealedSecret, WebhookFields},
};
use anyhow::Result;
use sqlx::PgPool;
pub async fn save(pool: &PgPool, hash: &str, manifest: &str, state: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM github_app_manifests WHERE expires_at<$1")
        .bind(chrono::Utc::now().timestamp())
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO github_app_manifests(hash,manifest,state,expires_at) VALUES($1,$2,$3,$4)",
    )
    .bind(hash)
    .bind(manifest)
    .bind(state)
    .bind(chrono::Utc::now().timestamp() + 600)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn draft(pool: &PgPool, hash: &str, approved: bool) -> Result<Option<Draft>> {
    Ok(sqlx::query_as("SELECT * FROM github_app_manifests WHERE ((NOT $2 AND hash=$1 AND user_id IS NULL) OR ($2 AND conversion_hash=$1 AND user_id IS NOT NULL)) AND expires_at>$3").bind(hash).bind(approved).bind(chrono::Utc::now().timestamp()).fetch_optional(pool).await?)
}
pub async fn approve(
    pool: &PgPool,
    hash: &str,
    actor: i64,
    session: &str,
    code: &str,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR SHARE")
        .bind(session)
        .fetch_optional(&mut *tx)
        .await?;
    let changed=sqlx::query("UPDATE github_app_manifests SET conversion_hash=$4,user_id=$2,session_id=$3,expires_at=$5 WHERE hash=$1 AND user_id IS NULL AND expires_at>$6 AND EXISTS(SELECT 1 FROM users u JOIN sessions s ON s.user_id=u.id WHERE u.id=$2 AND s.id=$3 AND s.expires_at>clock_timestamp() AND NOT u.is_banned AND NOT u.must_change_password)").bind(hash).bind(actor).bind(session).bind(code).bind(chrono::Utc::now().timestamp()+3600).bind(chrono::Utc::now().timestamp()).execute(&mut *tx).await?.rows_affected()==1;
    tx.commit().await?;
    Ok(changed)
}
pub async fn convert(
    pool: &PgPool,
    hash: &str,
    app: &App,
    secret_hash: &str,
    callbacks: &[String],
    hook: Option<&WebhookFields>,
    secret: Option<&SealedSecret>,
) -> Result<Option<App>> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    let candidate:Option<Draft>=sqlx::query_as("SELECT * FROM github_app_manifests WHERE conversion_hash=$1 AND user_id IS NOT NULL AND expires_at>$2").bind(hash).bind(chrono::Utc::now().timestamp()).fetch_optional(&mut *tx).await?;
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    let (Some(actor), Some(session)) = (candidate.user_id, candidate.session_id) else {
        return Ok(None);
    };
    // Account -> session -> grant, matching logout/account revocation and its FK cascades.
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR SHARE")
        .bind(&session)
        .fetch_optional(&mut *tx)
        .await?;
    let draft:Option<Draft>=sqlx::query_as("SELECT * FROM github_app_manifests WHERE conversion_hash=$1 AND user_id IS NOT NULL AND expires_at>$2 FOR UPDATE").bind(hash).bind(chrono::Utc::now().timestamp()).fetch_optional(&mut *tx).await?;
    let Some(draft) = draft else {
        return Ok(None);
    };
    if draft.user_id != Some(actor) || draft.session_id.as_deref() != Some(session.as_str()) {
        return Ok(None);
    }
    let Some(app) =
        super::github_apps::register_in_connection(&mut tx, actor, &session, app, secret_hash)
            .await?
    else {
        return Ok(None);
    };
    if let Some(callback) = callbacks.first() {
        sqlx::query("UPDATE oauth_applications SET redirect_uri=$2 WHERE client_id=$1")
            .bind(&app.client_id)
            .bind(callback)
            .execute(&mut *tx)
            .await?;
    }
    for callback in callbacks {
        sqlx::query(
            "INSERT INTO github_app_callbacks(app_id,uri) VALUES($1,$2) ON CONFLICT DO NOTHING",
        )
        .bind(app.id)
        .bind(callback)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(hook) = hook {
        super::github_app_hooks::save_in_connection(&mut tx, app.id, hook, secret, false).await?;
    }
    sqlx::query("DELETE FROM github_app_manifests WHERE hash=$1")
        .bind(&draft.hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(app))
}
pub async fn callback(pool: &PgPool, client: &str, uri: &str) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_callbacks c JOIN github_apps a ON a.id=c.app_id WHERE a.client_id=$1 AND c.uri=$2)").bind(client).bind(uri).fetch_one(pool).await?)
}
