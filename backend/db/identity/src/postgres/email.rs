use crate::email::delivery_allowed_sql;
use crate::email::primary_link_condition;
use crate::email::EmailLink;
use crate::email::NewEmailSettings;
use crate::email::RESEND_SECONDS;
use anyhow::Result;
use codeyard_database::models::EmailSettings;
use codeyard_database::models::OutboxMessage;
use codeyard_database::pool::PgPool;
use sqlx::PgConnection;

pub async fn get_email_settings(pool: &PgPool) -> Result<Option<EmailSettings>> {
    Ok(sqlx::query_as(
        "SELECT host,port,security,username,encrypted_password,password_nonce,from_name,from_address,reply_to
         FROM email_settings WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn save_email_settings(
    pool: &PgPool,
    actor: i64,
    session: &str,
    settings: &NewEmailSettings<'_>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    codeyard_database::queries::postgres::users::transitions::lock_accounts_shared(
        &mut tx,
        &[actor],
    )
    .await?;
    if !crate::postgres::users::transitions::active_admin(&mut tx, actor).await?
        || !codeyard_database::queries::postgres::users::transitions::live_session(
            &mut tx, actor, session,
        )
        .await?
    {
        return Ok(false);
    }
    let password = settings.password.filter(|_| !settings.username.is_empty());
    sqlx::query(
        r#"
        INSERT INTO email_settings
            (id, host, port, security, username, encrypted_password, password_nonce, from_name, from_address, reply_to)
        VALUES (1, $1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (id) DO UPDATE SET
            host = EXCLUDED.host,
            port = EXCLUDED.port,
            security = EXCLUDED.security,
            username = EXCLUDED.username,
            encrypted_password = CASE WHEN EXCLUDED.username='' THEN NULL ELSE COALESCE(EXCLUDED.encrypted_password, email_settings.encrypted_password) END,
            password_nonce = CASE WHEN EXCLUDED.username='' THEN NULL ELSE COALESCE(EXCLUDED.password_nonce, email_settings.password_nonce) END,
            from_name = EXCLUDED.from_name,
            from_address = EXCLUDED.from_address,
            reply_to = EXCLUDED.reply_to,
            updated_at = NOW()
        "#,
    )
    .bind(settings.host)
    .bind(settings.port)
    .bind(settings.security)
    .bind(settings.username)
    .bind(password.map(|(ciphertext, _)| ciphertext))
    .bind(password.map(|(_, nonce)| nonce))
    .bind(settings.from_name)
    .bind(settings.from_address)
    .bind(settings.reply_to)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,'node.email_settings','email_settings',1,$2)")
        .bind(actor)
        .bind(serde_json::json!({"host": settings.host, "password_changed": password.is_some() || settings.username.is_empty(), "authentication": !settings.username.is_empty()}))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn enqueue_email(
    pool: &PgPool,
    to_address: &str,
    subject: &str,
    body_text: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO email_outbox(to_address,subject,body_text) VALUES ($1,$2,$3)")
        .bind(to_address)
        .bind(subject)
        .bind(body_text)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn enqueue_notification_email(
    pool: &PgPool,
    notification: i64,
    to_address: &str,
    subject: &str,
    body_text: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO email_outbox(to_address,subject,body_text,notification_id) VALUES ($1,$2,$3,$4)",
    )
    .bind(to_address)
    .bind(subject)
    .bind(body_text)
    .bind(notification)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn withdraw_email(pool: &PgPool, id: i64, attempt: i32) -> Result<()> {
    sqlx::query("DELETE FROM email_outbox WHERE id=$1 AND sent_at IS NULL AND attempts=$2")
        .bind(id)
        .bind(attempt)
        .execute(pool)
        .await?;
    Ok(())
}

/// Queues a message in the caller's transaction, so it commits or rolls back with the change it
/// reports. Nothing is queued while the site has no mail server, as the outbox would never send
/// it. `invitation` links an organization invitation's offer to its message; the caller has
/// already written that invitation row, which keeps the invitation-before-outbox lock order.
pub async fn queue_email(
    connection: &mut PgConnection,
    to_address: &str,
    subject: &str,
    body_text: &str,
    invitation: Option<i64>,
) -> Result<()> {
    let saved: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM email_settings WHERE id=1)")
        .fetch_one(&mut *connection)
        .await?;
    if !saved && !codeyard_types::mail_config::environment_configured() {
        return Ok(());
    }
    sqlx::query("INSERT INTO email_outbox(to_address,subject,body_text,organization_invitation_id) VALUES ($1,$2,$3,$4)")
        .bind(to_address)
        .bind(subject)
        .bind(body_text)
        .bind(invitation)
        .execute(connection)
        .await?;
    Ok(())
}

pub async fn claim_due_email(
    pool: &PgPool,
    max_attempts: i32,
    lease_seconds: i64,
) -> Result<Option<OutboxMessage>> {
    let mut tx = pool.begin().await?;
    // A sender that died during the last attempt never recorded it, so the message is given up
    // once that claim runs out.
    sqlx::query("UPDATE organization_invitations SET failed_at=NOW(),failed_reason='Invitation email could not be delivered' WHERE failed_at IS NULL AND id IN (SELECT organization_invitation_id FROM email_outbox WHERE sent_at IS NULL AND attempts>=$1 AND body_text<>'' AND next_attempt_at<=NOW())")
        .bind(max_attempts).execute(&mut *tx).await?;
    sqlx::query("UPDATE email_outbox SET body_text='', last_error=COALESCE(last_error,'The send did not finish') WHERE sent_at IS NULL AND attempts>=$1 AND body_text<>'' AND next_attempt_at<=NOW()")
        .bind(max_attempts).execute(&mut *tx).await?;
    // Each claim counts as an attempt, so a send that never finishes still runs out of attempts.
    // SKIP LOCKED lets nodes that claim at once take different rows.
    let message = sqlx::query_as(
        r#"
        UPDATE email_outbox
        SET attempts = attempts + 1, next_attempt_at = NOW() + $2::float8 * INTERVAL '1 second'
        WHERE id = (
            SELECT id FROM email_outbox
            WHERE sent_at IS NULL AND attempts < $1 AND next_attempt_at <= NOW()
            ORDER BY next_attempt_at, id
            LIMIT 1
            FOR UPDATE SKIP LOCKED)
        RETURNING id, to_address, subject, body_text, attempts, notification_id
        "#,
    )
    .bind(max_attempts)
    .bind(lease_seconds as f64)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(message)
}

pub async fn delivery_allowed(pool: &PgPool, id: i64, attempt: i32) -> Result<bool> {
    Ok(
        sqlx::query_scalar(sqlx::AssertSqlSafe(delivery_allowed_sql(true)))
            .bind(id)
            .bind(attempt)
            .fetch_one(pool)
            .await?,
    )
}

pub async fn mark_email_sent(pool: &PgPool, id: i64, attempt: i32) -> Result<()> {
    sqlx::query(
        "UPDATE email_outbox SET sent_at = NOW(), last_error = NULL, body_text = '' WHERE id = $1 AND sent_at IS NULL AND attempts=$2",
    )
    .bind(id)
    .bind(attempt)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_email_failed(
    pool: &PgPool,
    id: i64,
    attempt: i32,
    error: &str,
    retry_seconds: i64,
    max_attempts: i32,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM organization_invitations WHERE id=(SELECT organization_invitation_id FROM email_outbox WHERE id=$1) FOR UPDATE").bind(id).execute(&mut *tx).await?;
    sqlx::query("UPDATE email_outbox SET last_error = $2, next_attempt_at = NOW() + $3::float8 * INTERVAL '1 second', body_text = CASE WHEN attempts >= $4 THEN '' ELSE body_text END WHERE id = $1 AND sent_at IS NULL AND attempts=$5")
        .bind(id)
        .bind(error)
        .bind(retry_seconds as f64)
        .bind(max_attempts)
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE organization_invitations SET failed_at=NOW(),failed_reason='Invitation email could not be delivered' WHERE failed_at IS NULL AND id IN (SELECT organization_invitation_id FROM email_outbox WHERE id=$1 AND attempts>=$2 AND attempts=$3 AND sent_at IS NULL)")
        .bind(id).bind(max_attempts).bind(attempt).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn remove_old_email(pool: &PgPool, max_attempts: i32, days: i64) -> Result<u64> {
    Ok(sqlx::query(
        "DELETE FROM email_outbox
         WHERE (sent_at IS NOT NULL OR attempts >= $1)
           AND created_at < NOW() - $2::float8 * INTERVAL '1 day'",
    )
    .bind(max_attempts)
    .bind(days as f64)
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn get_email_notifications(
    pool: &PgPool,
    user_id: i64,
    credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    crate::postgres::users::transitions::require_account(&mut tx, user_id, credential).await?;
    let enabled: bool = sqlx::query_scalar("SELECT email_notifications FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(enabled)
}

pub async fn set_email_notifications(
    pool: &PgPool,
    user_id: i64,
    credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>,
    enabled: bool,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    crate::postgres::users::transitions::require_account(&mut tx, user_id, credential).await?;
    sqlx::query("UPDATE users SET email_notifications = $2 WHERE id = $1")
        .bind(user_id)
        .bind(enabled)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn notification_email_address(pool: &PgPool, user_id: i64) -> Result<Option<String>> {
    let mut connection = pool.acquire().await?;
    notification_address(&mut connection, user_id).await
}

/// `notification_email_address` inside the caller's transaction.
pub async fn notification_address(
    connection: &mut PgConnection,
    user_id: i64,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT email FROM users
         WHERE id = $1 AND email_verified AND email_notifications AND NOT is_banned
           AND email IS NOT NULL AND email <> ''",
    )
    .bind(user_id)
    .fetch_optional(connection)
    .await?)
}

// Parent existence locks precede issuance locks, including when the issuance is replaced.
// Key share remains compatible with token consumers updating the account's non-key fields.
pub(super) async fn lock_link_recipient(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: i64,
) -> Result<bool> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE id=$1 FOR KEY SHARE")
            .bind(user)
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
    )
}

pub async fn save_email_token(pool: &PgPool, link: &EmailLink<'_>) -> Result<bool> {
    let mut tx = pool.begin().await?;
    codeyard_database::queries::postgres::users::transitions::lock_accounts_shared(
        &mut tx,
        &[link.user_id],
    )
    .await?;
    if !lock_link_recipient(&mut tx, link.user_id).await? {
        return Ok(false);
    }
    // A link goes only to the account's current address, and never to a banned account.
    let current: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id=$1 AND email=$2 AND NOT is_banned)",
    )
    .bind(link.user_id)
    .bind(link.email)
    .fetch_one(&mut *tx)
    .await?;
    if !current {
        return Ok(false);
    }
    // A link issued less than RESEND_SECONDS ago expires later than now + ttl - RESEND_SECONDS.
    let saved = sqlx::query(
        "INSERT INTO email_tokens(user_id,purpose,email,token_hash,expires_at)
         VALUES ($1,$2,$3,$4,NOW() + $5::float8 * INTERVAL '1 second')
         ON CONFLICT (user_id, purpose) DO UPDATE SET
            email = EXCLUDED.email,
            token_hash = EXCLUDED.token_hash,
            expires_at = EXCLUDED.expires_at
         WHERE lower(email_tokens.email) <> lower(EXCLUDED.email)
            OR email_tokens.expires_at <= NOW() + $6::float8 * INTERVAL '1 second'",
    )
    .bind(link.user_id)
    .bind(link.purpose)
    .bind(link.email)
    .bind(link.token_hash)
    .bind(link.ttl_seconds as f64)
    .bind((link.ttl_seconds - RESEND_SECONDS) as f64)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    // The mail carries this token, so it is queued only together with it.
    if saved {
        sqlx::query("DELETE FROM email_outbox WHERE link_user_id=$1 AND link_purpose=$2 AND sent_at IS NULL")
            .bind(link.user_id).bind(link.purpose).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO email_outbox(to_address,subject,body_text,link_user_id,link_purpose,link_token_hash) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(link.email).bind(link.subject).bind(link.body_text)
            .bind(link.user_id).bind(link.purpose).bind(link.token_hash)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(saved)
}

pub async fn email_token_is_valid(pool: &PgPool, purpose: &str, token_hash: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS(SELECT 1 FROM email_tokens t JOIN users u ON u.id=t.user_id WHERE t.purpose=$1 AND t.token_hash=$2 AND {})",
        primary_link_condition(true),
    )))
    .bind(purpose)
    .bind(token_hash)
    .fetch_one(pool)
    .await?)
}

pub async fn verify_email_token(pool: &PgPool, token_hash: &str) -> Result<Option<bool>> {
    let mut tx = pool.begin().await?;
    let user: Option<i64> = sqlx::query_scalar(
        "SELECT user_id FROM email_tokens WHERE purpose='verify' AND token_hash=$1",
    )
    .bind(token_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user) = user else { return Ok(None) };
    crate::postgres::users::transitions::lock_accounts(&mut tx, &[user]).await?;
    // Consumers retain the parent before taking the issuance, like issuers and account deletion.
    if sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE id=$1 FOR NO KEY UPDATE")
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let taken: Option<(i64, String)> = sqlx::query_as(
        "DELETE FROM email_tokens
         WHERE purpose = 'verify' AND token_hash = $1 AND user_id=$2 AND expires_at > NOW()
           AND EXISTS (SELECT 1 FROM users u
                       WHERE u.id = email_tokens.user_id AND u.email = email_tokens.email
                         AND NOT u.is_banned)
         RETURNING user_id, email::text",
    )
    .bind(token_hash)
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((user_id, email)) = taken else {
        return Ok(None);
    };
    let confirmed =
        sqlx::query("UPDATE users SET email_verified = TRUE WHERE id = $1 AND email = $2")
            .bind(user_id)
            .bind(&email)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
    tx.commit().await?;
    Ok(Some(confirmed))
}
