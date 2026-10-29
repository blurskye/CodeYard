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

pub async fn can_write_repository(pool: &SqlitePool, actor: i64, token: i64, repo: i64, permission: &str) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !super::super::discussions::lock_write_authority(&mut tx, actor, repo).await? {
        return Ok(false);
    }
    let allowed = permits_write(&mut tx, actor, token, Some(repo), None, permission).await?;
    tx.commit().await?;
    Ok(allowed)
}

/// Called inside the resource's BEGIN IMMEDIATE transaction, after account authority.
pub(crate) async fn permits_write(
    tx: &mut sqlx::SqliteConnection,
    actor: i64,
    token: i64,
    repo: Option<i64>,
    org: Option<i64>,
    permission: &str,
) -> Result<bool> {
    let live: Option<String> = sqlx::query_scalar("SELECT u.username FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=?1 AND u.id=?2 AND t.scope='write' AND (t.expires_at IS NULL OR t.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(token).bind(actor).fetch_optional(&mut *tx).await?;
    let Some(username) = live else { return Ok(false) };
    let repository: Option<crate::backend::db::models::Repository> = match repo {
        Some(repo) => sqlx::query_as("SELECT * FROM repositories WHERE id=?1").bind(repo).fetch_optional(&mut *tx).await?,
        None => None,
    };
    let org = org.or_else(|| repository.as_ref().and_then(|r| r.owner_org_id));
    let bounds: Option<crate::backend::db::fine_grained_tokens::Bounds> = sqlx::query_as(super::super::fine_grained_tokens::BOUNDS)
        .bind(token).fetch_optional(&mut *tx).await?;
    if let Some(bounds) = bounds {
        let active = bounds.active();
        let permissions: serde_json::Value = serde_json::from_str(&bounds.permissions)?;
        let reaches = if let Some(repository) = &repository {
            let selected: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM fine_grained_token_repositories WHERE token_id=?1 AND repo_id=?2)")
                .bind(token).bind(repository.id).fetch_one(&mut *tx).await?;
            (bounds.owner_org_id.is_some() && repository.owner_org_id == bounds.owner_org_id
                || bounds.owner_org_id.is_none() && repository.owner_user_id == Some(actor))
                && (bounds.repository_access == "all" || bounds.repository_access == "selected" && selected)
        } else {
            org.is_some() && bounds.owner_org_id == org
        };
        return Ok(active && reaches && crate::backend::identity::github_apps::allows(&permissions, permission, true));
    }
    let app_token: Option<crate::backend::db::github_apps::Token> = sqlx::query_as("SELECT * FROM github_app_tokens WHERE token_id=?1")
        .bind(token).fetch_optional(&mut *tx).await?;
    let oauth: Option<(String, String, bool)> = sqlx::query_as("SELECT o.client_id,o.scopes,COALESCE(c.is_github_app,FALSE) FROM oauth_app_tokens o LEFT JOIN oauth_applications c ON c.client_id=o.client_id WHERE o.token_id=?1")
        .bind(token).fetch_optional(&mut *tx).await?;
    if app_token.is_some() || oauth.as_ref().is_some_and(|o| o.2) {
        let Some(repo) = repo else { return Ok(false) };
        let (installation, permissions, selected) = if let Some(bound) = app_token {
            let selected: Vec<i64> = serde_json::from_str(&bound.repositories)?;
            (Some(bound.installation_id), bound.permissions, selected)
        } else {
            (None, String::new(), Vec::new())
        };
        let client = oauth.as_ref().map(|o| o.0.as_str());
        let grant: Option<String> = sqlx::query_scalar("SELECT a.permissions FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id JOIN repositories r ON r.id=?1 WHERE (?2 IS NULL OR i.id=?2) AND (?2 IS NOT NULL OR a.client_id=?3) AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id))")
            .bind(repo).bind(installation).bind(client).fetch_optional(&mut *tx).await?;
        let Some(grant) = grant else { return Ok(false) };
        let grant: serde_json::Value = serde_json::from_str(&grant)?;
        let permissions: serde_json::Value = if installation.is_some() { serde_json::from_str(&permissions)? } else { grant.clone() };
        // App environment administration and fine-grained environment grants use different names.
        let permission = if permission == "environments" { "administration" } else { permission };
        return Ok((selected.is_empty() || selected.contains(&repo))
            && crate::backend::identity::github_apps::allows(&permissions, permission, true)
            && crate::backend::identity::github_apps::allows(&grant, permission, true));
    }
    if username == crate::backend::db::ci_tokens::BOT {
        let job: Option<(i64, String)> = sqlx::query_as("SELECT r.repo_id,t.permissions FROM actions_github_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.token_id=?1 AND t.revoked_at IS NULL AND r.status='running'")
            .bind(token).fetch_optional(&mut *tx).await?;
        return match job {
            Some((id, permissions)) => Ok(repo == Some(id) && serde_json::from_str::<crate::backend::ci::tokens::Permissions>(&permissions)?.allows(&permission.replace('_', "-"), true)),
            None => Ok(false),
        };
    }
    if let Some((client, scopes, _)) = oauth {
        let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oauth_applications c JOIN users owner ON owner.id=c.owner_id WHERE c.client_id=?1 AND NOT owner.is_banned AND NOT owner.must_change_password)")
            .bind(&client).fetch_one(&mut *tx).await?;
        let allowed = crate::backend::identity::oauth_apps::has_scope(&scopes, if repo.is_some() { "repo" } else { "write:org" })
            || permission == "security_events" && crate::backend::identity::oauth_apps::has_scope(&scopes, "security_events");
        return Ok((active || client == crate::backend::identity::oauth_apps::CLI_CLIENT) && allowed);
    }
    let refused: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_token_policies WHERE org_id=?1 AND NOT classic_allowed)")
        .bind(org).fetch_one(&mut *tx).await?;
    Ok(!refused)
}

pub async fn touch_token(pool: &SqlitePool, token_id: i64, address: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE personal_access_tokens SET last_used_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now'),last_used_ip=?2 WHERE id=?1")
        .bind(token_id)
        .bind(address)
        .execute(pool)
        .await?;
    Ok(())
}
