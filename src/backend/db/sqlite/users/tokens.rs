use crate::backend::db::{
    models::{PersonalAccessToken, User},
    sqlite::time,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

pub async fn create_token(
    pool: &SqlitePool,
    user_id: i64,
    session_id: &str,
    name: &str,
    hash: &str,
    scope: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Result<Option<PersonalAccessToken>> {
    // One statement is atomic: its session and account checks read the same state as
    // its insert. A password change that committed first makes the checks fail.
    Ok(sqlx::query_as("INSERT INTO personal_access_tokens(user_id,name,token_hash,scope,expires_at) SELECT id,?2,?3,?4,?5 FROM users WHERE id=?1 AND NOT is_banned AND NOT must_change_password AND EXISTS(SELECT 1 FROM sessions WHERE id=?6 AND user_id=?1 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) RETURNING id,user_id,name,scope,expires_at,created_at")
        .bind(user_id).bind(name).bind(hash).bind(scope).bind(expires_at.map(time::encode)).bind(session_id).fetch_optional(pool).await?)
}

pub async fn list_tokens(pool: &SqlitePool, user_id: i64) -> Result<Vec<PersonalAccessToken>> {
    Ok(sqlx::query_as("SELECT id,user_id,name,scope,expires_at,created_at FROM personal_access_tokens WHERE user_id=?1 ORDER BY created_at DESC,id DESC")
        .bind(user_id).fetch_all(pool).await?)
}

pub async fn delete_token(pool: &SqlitePool, user_id: i64, token_id: i64) -> Result<bool> {
    Ok(sqlx::query("DELETE FROM personal_access_tokens WHERE id=?1 AND user_id=?2 AND EXISTS(SELECT 1 FROM users WHERE id=?2 AND NOT is_banned AND NOT must_change_password)")
        .bind(token_id).bind(user_id).execute(pool).await?.rows_affected() == 1)
}

pub async fn find_token(pool: &SqlitePool, hash: &str) -> Result<Option<PersonalAccessToken>> {
    Ok(sqlx::query_as("SELECT id,user_id,name,scope,expires_at,created_at FROM personal_access_tokens WHERE token_hash=?1")
        .bind(hash).fetch_optional(pool).await?)
}

pub async fn token_user(pool: &SqlitePool, token_id: i64, write: bool) -> Result<Option<User>> {
    Ok(sqlx::query_as("SELECT u.* FROM users u JOIN personal_access_tokens t ON t.user_id=u.id WHERE t.id=?1 AND (t.expires_at IS NULL OR t.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) AND (NOT ?2 OR t.scope='write') AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(token_id).bind(write).fetch_optional(pool).await?)
}
