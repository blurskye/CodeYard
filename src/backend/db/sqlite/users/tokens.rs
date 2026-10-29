use crate::backend::db::{
    models::{PersonalAccessToken, User},
    sqlite::time,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

/// The initiating credential is checked inside the same transaction as its repository write.
/// This only narrows authority; the caller must also hold native repository write authority.
pub(crate) async fn repository_write_on(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    token: i64,
    actor: i64,
    repo: i64,
    permission: &str,
) -> Result<bool> {
    use crate::backend::{db::fine_grained_tokens::Bounds, identity::{fine_grained, github_apps, oauth_apps}};
    let live: Option<String> = sqlx::query_scalar("SELECT u.username FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND t.user_id=$2 AND t.scope='write' AND (t.expires_at IS NULL OR datetime(t.expires_at)>datetime('now')) AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(token).bind(actor).fetch_optional(&mut **tx).await?;
    let Some(username) = live else { return Ok(false); };
    if username == crate::backend::db::ci_tokens::BOT {
        let job: Option<(i64, String)> = sqlx::query_as("SELECT r.repo_id,t.permissions FROM actions_github_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.token_id=$1 AND t.revoked_at IS NULL AND r.status='running'")
            .bind(token).fetch_optional(&mut **tx).await?;
        return Ok(match job {
            Some((id, permissions)) => id == repo && serde_json::from_str::<crate::backend::ci::tokens::Permissions>(&permissions)?.allows(permission, true),
            None => false,
        });
    }
    let bounds: Option<Bounds> = sqlx::query_as(super::super::fine_grained_tokens::BOUNDS)
        .bind(token).fetch_optional(&mut **tx).await?;
    if let Some(bounds) = bounds {
        let selected: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r WHERE r.id=$1 AND NOT r.is_gist AND (($2 IS NULL AND r.owner_user_id=$3) OR r.owner_org_id=$2) AND ($4='all' OR ($4='selected' AND EXISTS(SELECT 1 FROM fine_grained_token_repositories s WHERE s.token_id=$5 AND s.repo_id=r.id))))")
            .bind(repo).bind(bounds.owner_org_id).bind(actor).bind(&bounds.repository_access).bind(token).fetch_one(&mut **tx).await?;
        let active = bounds.active();
        let permissions = serde_json::from_str(&bounds.permissions)?;
        return Ok(active && selected && fine_grained::allows(&permissions, &permission.replace('-', "_"), true));
    }
    let bound: Option<(i64, String, String)> = sqlx::query_as("SELECT installation_id,permissions,repositories FROM github_app_tokens WHERE token_id=$1")
        .bind(token).fetch_optional(&mut **tx).await?;
    if let Some((installation, permissions, repositories)) = bound {
        let selected: Vec<i64> = serde_json::from_str(&repositories)?;
        let covers: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users u ON u.id=a.owner_id JOIN repositories r ON r.id=$2 WHERE i.id=$1 AND i.suspended_at IS NULL AND NOT u.is_banned AND NOT u.must_change_password AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)))")
            .bind(installation).bind(repo).fetch_one(&mut **tx).await?;
        return Ok(covers && (selected.is_empty() || selected.contains(&repo)) && github_apps::allows(&serde_json::from_str(&permissions)?, &permission.replace('-', "_"), true));
    }
    let oauth: Option<(String, String)> = sqlx::query_as("SELECT client_id,scopes FROM oauth_app_tokens WHERE token_id=$1")
        .bind(token).fetch_optional(&mut **tx).await?;
    if let Some((client, scopes)) = oauth {
        let app: Option<(i64, String)> = sqlx::query_as("SELECT a.id,a.permissions FROM github_apps a JOIN users u ON u.id=a.owner_id WHERE a.client_id=$1 AND NOT u.is_banned AND NOT u.must_change_password")
            .bind(&client).fetch_optional(&mut **tx).await?;
        if let Some((app, permissions)) = app {
            let covers: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_installations i JOIN repositories r ON r.id=$2 WHERE i.app_id=$1 AND i.suspended_at IS NULL AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)))")
                .bind(app).bind(repo).fetch_one(&mut **tx).await?;
            return Ok(covers && github_apps::allows(&serde_json::from_str(&permissions)?, &permission.replace('-', "_"), true));
        }
        let exists: bool = sqlx::query_scalar("SELECT $1=$2 OR EXISTS(SELECT 1 FROM oauth_applications WHERE client_id=$1)")
            .bind(client).bind(oauth_apps::CLI_CLIENT).fetch_one(&mut **tx).await?;
        return Ok(exists && oauth_apps::allows(&scopes, "/repos/owner/repo", true));
    }
    let refused: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r JOIN organization_token_policies p ON p.org_id=r.owner_org_id WHERE r.id=$1 AND NOT p.classic_allowed)")
        .bind(repo).fetch_one(&mut **tx).await?;
    Ok(!refused)
}

pub async fn repository_write(pool: &SqlitePool, token: i64, actor: i64, repo: i64, permission: &str) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let allowed = repository_write_on(&mut tx, token, actor, repo, permission).await?;
    tx.commit().await?;
    Ok(allowed)
}

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
