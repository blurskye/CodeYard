use super::models::{EmailSettings, OutboxMessage};
use crate::backend::db::email::{NewEmailSettings, RESEND_SECONDS};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn get_email_settings(pool: &SqlitePool) -> Result<Option<EmailSettings>> {
    Ok(sqlx::query_as(
        "SELECT host,port,security,username,encrypted_password,password_nonce,from_name,from_address,reply_to
         FROM email_settings WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn save_email_settings(
    pool: &SqlitePool,
    actor: i64,
    session: &str,
    settings: &NewEmailSettings<'_>,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !super::users::transitions::active_admin(&mut tx, actor).await?
        || !super::users::transitions::live_session(&mut tx, actor, session).await?
    {
        return Ok(false);
    }
    sqlx::query(
        r#"
        INSERT INTO email_settings
            (id, host, port, security, username, encrypted_password, password_nonce, from_name, from_address, reply_to)
        VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ON CONFLICT (id) DO UPDATE SET
            host = EXCLUDED.host,
            port = EXCLUDED.port,
            security = EXCLUDED.security,
            username = EXCLUDED.username,
            encrypted_password = COALESCE(EXCLUDED.encrypted_password, email_settings.encrypted_password),
            password_nonce = COALESCE(EXCLUDED.password_nonce, email_settings.password_nonce),
            from_name = EXCLUDED.from_name,
            from_address = EXCLUDED.from_address,
            reply_to = EXCLUDED.reply_to,
            updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
        "#,
    )
    .bind(settings.host)
    .bind(settings.port)
    .bind(settings.security)
    .bind(settings.username)
    .bind(settings.password.map(|(ciphertext, _)| ciphertext))
    .bind(settings.password.map(|(_, nonce)| nonce))
    .bind(settings.from_name)
    .bind(settings.from_address)
    .bind(settings.reply_to)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES(?1,'node.email_settings','email_settings',1,?2)")
        .bind(actor)
        .bind(serde_json::json!({"host": settings.host, "password_changed": settings.password.is_some()}).to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn enqueue_email(
    pool: &SqlitePool,
    to_address: &str,
    subject: &str,
    body_text: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO email_outbox(to_address,subject,body_text) VALUES (?1,?2,?3)")
        .bind(to_address)
        .bind(subject)
        .bind(body_text)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn claim_due_email(
    pool: &SqlitePool,
    max_attempts: i32,
    lease_seconds: i64,
) -> Result<Option<OutboxMessage>> {
    // One statement, so two nodes cannot take the same row.
    Ok(sqlx::query_as(
        r#"
        UPDATE email_outbox
        SET attempts = attempts + 1,
            next_attempt_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now', ?2 || ' seconds')
        WHERE id = (
            SELECT id FROM email_outbox
            WHERE sent_at IS NULL AND attempts < ?1
              AND next_attempt_at <= strftime('%Y-%m-%dT%H:%M:%f000Z','now')
            ORDER BY next_attempt_at, id
            LIMIT 1)
        RETURNING id, to_address, subject, body_text, attempts
        "#,
    )
    .bind(max_attempts)
    .bind(lease_seconds)
    .fetch_optional(pool)
    .await?)
}

pub async fn mark_email_sent(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("UPDATE email_outbox SET sent_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now'), last_error = NULL, body_text = '' WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_email_failed(
    pool: &SqlitePool,
    id: i64,
    error: &str,
    retry_seconds: i64,
    max_attempts: i32,
) -> Result<()> {
    sqlx::query("UPDATE email_outbox SET last_error = ?2, next_attempt_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now', ?3 || ' seconds'), body_text = CASE WHEN attempts >= ?4 THEN '' ELSE body_text END WHERE id = ?1")
        .bind(id)
        .bind(error)
        .bind(retry_seconds)
        .bind(max_attempts)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn remove_old_email(pool: &SqlitePool, max_attempts: i32, days: i64) -> Result<u64> {
    Ok(sqlx::query(
        "DELETE FROM email_outbox
         WHERE (sent_at IS NOT NULL OR attempts >= ?1)
           AND created_at < strftime('%Y-%m-%dT%H:%M:%f000Z','now', '-' || ?2 || ' days')",
    )
    .bind(max_attempts)
    .bind(days)
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn get_email_notifications(pool: &SqlitePool, user_id: i64) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT email_notifications FROM users WHERE id = ?1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or(true),
    )
}

pub async fn set_email_notifications(pool: &SqlitePool, user_id: i64, enabled: bool) -> Result<()> {
    sqlx::query("UPDATE users SET email_notifications = ?2 WHERE id = ?1")
        .bind(user_id)
        .bind(enabled)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn notification_email_address(pool: &SqlitePool, user_id: i64) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT email FROM users
         WHERE id = ?1 AND email_verified AND email_notifications AND NOT is_banned
           AND email IS NOT NULL AND email <> ''",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn save_email_token(
    pool: &SqlitePool,
    user_id: i64,
    purpose: &str,
    email: &str,
    token_hash: &str,
    ttl_seconds: i64,
) -> Result<bool> {
    // A link issued less than RESEND_SECONDS ago expires later than now + ttl - RESEND_SECONDS.
    Ok(sqlx::query(
        "INSERT INTO email_tokens(user_id,purpose,email,token_hash,expires_at)
         VALUES (?1,?2,?3,?4,strftime('%Y-%m-%dT%H:%M:%f000Z','now', ?5 || ' seconds'))
         ON CONFLICT (user_id, purpose) DO UPDATE SET
            email = EXCLUDED.email,
            token_hash = EXCLUDED.token_hash,
            expires_at = EXCLUDED.expires_at
         WHERE lower(email_tokens.email) <> lower(EXCLUDED.email)
            OR email_tokens.expires_at <= strftime('%Y-%m-%dT%H:%M:%f000Z','now', ?6 || ' seconds')",
    )
    .bind(user_id)
    .bind(purpose)
    .bind(email)
    .bind(token_hash)
    .bind(ttl_seconds)
    .bind(ttl_seconds - RESEND_SECONDS)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn email_token_is_valid(
    pool: &SqlitePool,
    purpose: &str,
    token_hash: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM email_tokens t JOIN users u ON u.id = t.user_id
            WHERE t.purpose = ?1 AND t.token_hash = ?2
              AND t.expires_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now')
              AND u.email = t.email AND NOT u.is_banned)",
    )
    .bind(purpose)
    .bind(token_hash)
    .fetch_one(pool)
    .await?)
}

pub async fn take_email_token(
    pool: &SqlitePool,
    purpose: &str,
    token_hash: &str,
) -> Result<Option<(i64, String)>> {
    Ok(sqlx::query_as(
        "DELETE FROM email_tokens
         WHERE purpose = ?1 AND token_hash = ?2
           AND expires_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now')
           AND EXISTS (SELECT 1 FROM users u
                       WHERE u.id = email_tokens.user_id AND u.email = email_tokens.email
                         AND NOT u.is_banned)
         RETURNING user_id, email",
    )
    .bind(purpose)
    .bind(token_hash)
    .fetch_optional(pool)
    .await?)
}

pub async fn confirm_email(pool: &SqlitePool, user_id: i64, email: &str) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE users SET email_verified = TRUE WHERE id = ?1 AND email = ?2")
            .bind(user_id)
            .bind(email)
            .execute(pool)
            .await?
            .rows_affected()
            == 1,
    )
}
