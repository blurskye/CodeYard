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

/// The newest 100 tokens older than `before`, or the newest 100 when it is 0.
pub async fn list_tokens(
    pool: &SqlitePool,
    user_id: i64,
    before: i64,
) -> Result<Vec<PersonalAccessToken>> {
    Ok(sqlx::query_as("SELECT id,user_id,name,scope,expires_at,created_at,last_used_at,last_used_ip FROM personal_access_tokens WHERE user_id=?1 AND (?2<=0 OR id<?2) AND NOT EXISTS(SELECT 1 FROM fine_grained_tokens f WHERE f.token_id=personal_access_tokens.id) ORDER BY id DESC LIMIT 100")
        .bind(user_id).bind(before).fetch_all(pool).await?)
}

pub async fn delete_token(
    pool: &SqlitePool,
    user_id: i64,
    session_id: &str,
    token_id: i64,
) -> Result<bool> {
    Ok(sqlx::query("DELETE FROM personal_access_tokens WHERE id=?1 AND user_id=?2 AND EXISTS(SELECT 1 FROM users WHERE id=?2 AND NOT is_banned AND NOT must_change_password) AND EXISTS(SELECT 1 FROM sessions WHERE id=?3 AND user_id=?2 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now'))")
        .bind(token_id).bind(user_id).bind(session_id).execute(pool).await?.rows_affected() == 1)
}

pub async fn find_token(pool: &SqlitePool, hash: &str) -> Result<Option<PersonalAccessToken>> {
    Ok(sqlx::query_as("SELECT id,user_id,name,scope,expires_at,created_at,last_used_at,last_used_ip FROM personal_access_tokens WHERE token_hash=?1")
        .bind(hash).fetch_optional(pool).await?)
}

pub async fn token_user(pool: &SqlitePool, token_id: i64, write: bool) -> Result<Option<User>> {
    // A removed OAuth client invalidates its tokens in every transport, including feeds.
    let user: Option<User> = sqlx::query_as("SELECT u.* FROM users u JOIN personal_access_tokens t ON t.user_id=u.id WHERE t.id=?1 AND (t.expires_at IS NULL OR t.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) AND (NOT ?2 OR t.scope='write') AND NOT u.is_banned AND NOT u.must_change_password AND NOT EXISTS(SELECT 1 FROM oauth_app_tokens o WHERE o.token_id=t.id AND o.client_id<>?3 AND NOT EXISTS(SELECT 1 FROM oauth_applications a WHERE a.client_id=o.client_id)) AND NOT EXISTS(SELECT 1 FROM github_app_tokens g WHERE g.token_id=t.id AND NOT EXISTS(SELECT 1 FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users owner ON owner.id=a.owner_id WHERE i.id=g.installation_id AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password)) AND NOT EXISTS(SELECT 1 FROM oauth_app_tokens o JOIN oauth_applications c ON c.client_id=o.client_id WHERE o.token_id=t.id AND c.is_github_app AND NOT EXISTS(SELECT 1 FROM github_apps a JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id WHERE a.client_id=c.client_id AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password))")
        .bind(token_id).bind(write).bind(crate::backend::identity::oauth_apps::CLI_CLIENT).fetch_optional(pool).await?;
    if user.as_ref().is_some_and(|user| {
        user.username.ends_with("[bot]") && user.username != crate::backend::db::ci_tokens::BOT
    }) && super::super::github_apps::token(pool, token_id)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    if user
        .as_ref()
        .is_some_and(|user| user.username == crate::backend::db::ci_tokens::BOT)
        && super::super::ci_tokens::get(pool, token_id)
            .await?
            .is_none()
    {
        return Ok(None);
    }
    Ok(user)
}

pub async fn touch_token(pool: &SqlitePool, token_id: i64, address: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE personal_access_tokens SET last_used_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now'),last_used_ip=?2 WHERE id=?1")
        .bind(token_id)
        .bind(address)
        .execute(pool)
        .await?;
    Ok(())
}
