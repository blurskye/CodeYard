use crate::backend::db::models::{PersonalAccessToken, User};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

pub async fn create_token(
    pool: &PgPool,
    user_id: i64,
    session_id: &str,
    name: &str,
    hash: &str,
    scope: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Result<Option<PersonalAccessToken>> {
    let mut tx = pool.begin().await?;
    // Serialize creation with account suspension and password-reset revocation.
    let active = sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password FOR UPDATE")
        .bind(user_id).fetch_optional(&mut *tx).await?;
    if active.is_none() {
        return Ok(None);
    }
    // Its own statement, on purpose: under READ COMMITTED it sees any password change
    // that committed while this call waited for the lock above.
    let expiry: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT expires_at FROM sessions WHERE id=$1 AND user_id=$2 FOR SHARE")
            .bind(session_id)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let live: bool = sqlx::query_scalar("SELECT COALESCE($1::timestamptz>clock_timestamp(),FALSE)")
        .bind(expiry)
        .fetch_one(&mut *tx)
        .await?;
    if !live {
        return Ok(None);
    }
    let token = sqlx::query_as("INSERT INTO personal_access_tokens(user_id,name,token_hash,scope,expires_at) VALUES($1,$2,$3,$4,$5) RETURNING id,user_id,name,scope,expires_at,created_at")
        .bind(user_id).bind(name).bind(hash).bind(scope).bind(expires_at).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(token))
}

pub async fn list_tokens(pool: &PgPool, user_id: i64) -> Result<Vec<PersonalAccessToken>> {
    Ok(sqlx::query_as("SELECT id,user_id,name,scope,expires_at,created_at FROM personal_access_tokens WHERE user_id=$1 ORDER BY created_at DESC,id DESC")
        .bind(user_id).fetch_all(pool).await?)
}

pub async fn delete_token(pool: &PgPool, user_id: i64, token_id: i64) -> Result<bool> {
    Ok(sqlx::query("DELETE FROM personal_access_tokens WHERE id=$1 AND user_id=$2 AND EXISTS(SELECT 1 FROM users WHERE id=$2 AND NOT is_banned AND NOT must_change_password)")
        .bind(token_id).bind(user_id).execute(pool).await?.rows_affected() == 1)
}

// Include expired and suspended tokens here so failures share the account's login budget.
pub async fn find_token(pool: &PgPool, hash: &str) -> Result<Option<PersonalAccessToken>> {
    Ok(sqlx::query_as("SELECT id,user_id,name,scope,expires_at,created_at FROM personal_access_tokens WHERE token_hash=$1")
        .bind(hash).fetch_optional(pool).await?)
}

pub async fn token_user(pool: &PgPool, token_id: i64, write: bool) -> Result<Option<User>> {
    Ok(sqlx::query_as("SELECT u.* FROM users u JOIN personal_access_tokens t ON t.user_id=u.id WHERE t.id=$1 AND (t.expires_at IS NULL OR t.expires_at>clock_timestamp()) AND (NOT $2 OR t.scope='write') AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(token_id).bind(write).fetch_optional(pool).await?)
}
