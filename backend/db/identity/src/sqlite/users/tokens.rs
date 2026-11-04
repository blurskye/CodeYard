use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::models::PersonalAccessToken;
use codeyard_db_core::models::User;
use codeyard_db_core::queries::sqlite::users::tokens::permits_write;
use sqlx::SqlitePool;

/// The initiating credential is checked inside the same transaction as its repository write.
/// This only narrows authority; the caller must also hold native repository write authority.
pub async fn repository_write_on(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    token: i64,
    actor: i64,
    repo: i64,
    permission: &str,
) -> Result<bool> {
    use codeyard_db_core::queries::fine_grained_tokens::Bounds;

    let live: Option<String> = sqlx::query_scalar("SELECT u.username FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND t.user_id=$2 AND t.scope='write' AND (t.expires_at IS NULL OR datetime(t.expires_at)>datetime('now')) AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(token).bind(actor).fetch_optional(&mut **tx).await?;
    let Some(username) = live else {
        return Ok(false);
    };
    if username == codeyard_db_core::queries::ci_tokens::BOT {
        let job: Option<(i64, String)> = sqlx::query_as("SELECT r.repo_id,t.permissions FROM actions_github_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.token_id=$1 AND t.revoked_at IS NULL AND r.status='running'")
            .bind(token).fetch_optional(&mut **tx).await?;
        return Ok(match job {
            Some((id, permissions)) => {
                id == repo
                    && serde_json::from_str::<codeyard_model::permissions::Permissions>(
                        &permissions,
                    )?
                    .allows(permission, true)
            }
            None => false,
        });
    }
    let bounds: Option<Bounds> =
        sqlx::query_as(codeyard_db_core::queries::sqlite::fine_grained_tokens::BOUNDS)
            .bind(token)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(bounds) = bounds {
        let selected: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r WHERE r.id=$1 AND NOT r.is_gist AND (($2 IS NULL AND r.owner_user_id=$3) OR r.owner_org_id=$2) AND ($4='all' OR ($4='selected' AND EXISTS(SELECT 1 FROM fine_grained_token_repositories s WHERE s.token_id=$5 AND s.repo_id=r.id))))")
            .bind(repo).bind(bounds.owner_org_id).bind(actor).bind(&bounds.repository_access).bind(token).fetch_one(&mut **tx).await?;
        let active = bounds.active();
        let permissions = serde_json::from_str(&bounds.permissions)?;
        return Ok(active
            && selected
            && codeyard_model::fine_grained::allows(
                &permissions,
                &permission.replace('-', "_"),
                true,
            ));
    }
    let bound: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT installation_id,permissions,repositories FROM github_app_tokens WHERE token_id=$1",
    )
    .bind(token)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((installation, permissions, repositories)) = bound {
        let selected: Vec<i64> = serde_json::from_str(&repositories)?;
        let covers: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users u ON u.id=a.owner_id JOIN repositories r ON r.id=$2 WHERE i.id=$1 AND i.suspended_at IS NULL AND NOT u.is_banned AND NOT u.must_change_password AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)))")
            .bind(installation).bind(repo).fetch_one(&mut **tx).await?;
        return Ok(covers
            && (selected.is_empty() || selected.contains(&repo))
            && codeyard_model::github_apps::allows(
                &serde_json::from_str(&permissions)?,
                &permission.replace('-', "_"),
                true,
            ));
    }
    let oauth: Option<(String, String)> =
        sqlx::query_as("SELECT client_id,scopes FROM oauth_app_tokens WHERE token_id=$1")
            .bind(token)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some((client, scopes)) = oauth {
        let app: Option<(i64, String)> = sqlx::query_as("SELECT a.id,a.permissions FROM github_apps a JOIN users u ON u.id=a.owner_id WHERE a.client_id=$1 AND NOT u.is_banned AND NOT u.must_change_password")
            .bind(&client).fetch_optional(&mut **tx).await?;
        if let Some((app, permissions)) = app {
            let covers: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_installations i JOIN repositories r ON r.id=$2 WHERE i.app_id=$1 AND i.suspended_at IS NULL AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)))")
                .bind(app).bind(repo).fetch_one(&mut **tx).await?;
            return Ok(covers
                && codeyard_model::github_apps::allows(
                    &serde_json::from_str(&permissions)?,
                    &permission.replace('-', "_"),
                    true,
                ));
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT $1=$2 OR EXISTS(SELECT 1 FROM oauth_applications WHERE client_id=$1)",
        )
        .bind(client)
        .bind(codeyard_model::identity::CLI_CLIENT)
        .fetch_one(&mut **tx)
        .await?;
        return Ok(exists && codeyard_model::oauth_apps::allows(&scopes, "/repos/owner/repo", true));
    }
    let refused: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r JOIN organization_token_policies p ON p.org_id=r.owner_org_id WHERE r.id=$1 AND NOT p.classic_allowed)")
        .bind(repo).fetch_one(&mut **tx).await?;
    Ok(!refused)
}

/// Organization team writes retain the initiating credential alongside native organization authority.
pub async fn team_write_on(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    token: i64,
    actor: i64,
    org: i64,
) -> Result<bool> {
    use codeyard_db_core::queries::fine_grained_tokens::Bounds;

    let live: Option<i64> = sqlx::query_scalar("SELECT t.id FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=?1 AND t.user_id=?2 AND t.scope='write' AND (t.expires_at IS NULL OR datetime(t.expires_at)>datetime('now')) AND NOT u.is_banned AND NOT u.must_change_password AND u.username<>?3 AND NOT EXISTS(SELECT 1 FROM actions_github_tokens j WHERE j.token_id=t.id)").bind(token).bind(actor).bind(codeyard_db_core::queries::ci_tokens::BOT).fetch_optional(&mut **tx).await?;
    if live.is_none() {
        return Ok(false);
    }
    let bounds: Option<Bounds> =
        sqlx::query_as(codeyard_db_core::queries::sqlite::fine_grained_tokens::BOUNDS)
            .bind(token)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(bounds) = bounds {
        return Ok(bounds.active()
            && bounds.owner_org_id == Some(org)
            && codeyard_model::fine_grained::allows(
                &serde_json::from_str(&bounds.permissions)?,
                "members",
                true,
            ));
    }
    let bound: Option<(i64, String)> = sqlx::query_as(
        "SELECT installation_id,permissions FROM github_app_tokens WHERE token_id=?1",
    )
    .bind(token)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((installation, permissions)) = bound {
        let covers: Option<i64> = sqlx::query_scalar("SELECT i.id FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users u ON u.id=a.owner_id WHERE i.id=?1 AND i.org_id=?2 AND i.suspended_at IS NULL AND NOT u.is_banned AND NOT u.must_change_password").bind(installation).bind(org).fetch_optional(&mut **tx).await?;
        return Ok(covers.is_some()
            && codeyard_model::github_apps::allows(
                &serde_json::from_str(&permissions)?,
                "members",
                true,
            ));
    }
    let oauth: Option<(String, String)> =
        sqlx::query_as("SELECT client_id,scopes FROM oauth_app_tokens WHERE token_id=?1")
            .bind(token)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some((client, scopes)) = oauth {
        let app: Option<(i64, String)> = sqlx::query_as("SELECT a.id,a.permissions FROM github_apps a JOIN users u ON u.id=a.owner_id WHERE a.client_id=?1 AND NOT u.is_banned AND NOT u.must_change_password").bind(&client).fetch_optional(&mut **tx).await?;
        if let Some((app, permissions)) = app {
            let covers: Option<i64> = sqlx::query_scalar("SELECT id FROM github_app_installations WHERE app_id=?1 AND org_id=?2 AND suspended_at IS NULL").bind(app).bind(org).fetch_optional(&mut **tx).await?;
            return Ok(covers.is_some()
                && codeyard_model::github_apps::allows(
                    &serde_json::from_str(&permissions)?,
                    "members",
                    true,
                ));
        }
        let client_exists = client == codeyard_model::identity::CLI_CLIENT
            || sqlx::query_scalar::<_, i64>("SELECT id FROM oauth_applications WHERE client_id=?1")
                .bind(client)
                .fetch_optional(&mut **tx)
                .await?
                .is_some();
        return Ok(client_exists && codeyard_model::oauth_apps::has_scope(&scopes, "write:org"));
    }
    let policy: Option<bool> = sqlx::query_scalar(
        "SELECT classic_allowed FROM organization_token_policies WHERE org_id=?1",
    )
    .bind(org)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(policy.unwrap_or(true))
}

pub async fn repository_write(
    pool: &SqlitePool,
    token: i64,
    actor: i64,
    repo: i64,
    permission: &str,
) -> Result<bool> {
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
    anyhow::ensure!(
        codeyard_db_core::queries::users::tokens::supported_expiry(expires_at),
        "Token expiry is outside the supported calendar range"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !crate::sqlite::users::transitions::active_session(&mut tx, user_id, session_id).await? {
        return Ok(None);
    }
    let token = sqlx::query_as("INSERT INTO personal_access_tokens(user_id,name,token_hash,scope,expires_at) VALUES(?1,?2,?3,?4,?5) RETURNING id,user_id,name,scope,expires_at,created_at")
        .bind(user_id).bind(name).bind(hash).bind(scope).bind(expires_at.map(codeyard_db_core::queries::sqlite::time::encode)).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(token))
}

/// The newest 100 tokens older than `before`, or the newest 100 when it is 0.
pub async fn list_tokens(
    pool: &SqlitePool,
    user_id: i64,
    session_id: &str,
    before: i64,
) -> Result<Vec<PersonalAccessToken>> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::sqlite::users::transitions::active_session_read(&mut tx, user_id, session_id)
            .await?,
        codeyard_support::credentials::AccountRestricted
    );
    let tokens = sqlx::query_as("SELECT id,user_id,name,scope,expires_at,created_at,last_used_at,last_used_ip FROM personal_access_tokens WHERE user_id=?1 AND (?2<=0 OR id<?2) AND NOT EXISTS(SELECT 1 FROM fine_grained_tokens f WHERE f.token_id=personal_access_tokens.id) ORDER BY id DESC LIMIT 100")
        .bind(user_id).bind(before).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(tokens)
}

pub async fn delete_token(
    pool: &SqlitePool,
    user_id: i64,
    session_id: &str,
    token_id: i64,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !crate::sqlite::users::transitions::active_session(&mut tx, user_id, session_id).await? {
        return Ok(false);
    }
    let deleted = sqlx::query("DELETE FROM personal_access_tokens WHERE id=?1 AND user_id=?2")
        .bind(token_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(deleted)
}

pub async fn find_token(pool: &SqlitePool, hash: &str) -> Result<Option<PersonalAccessToken>> {
    Ok(sqlx::query_as("SELECT id,user_id,name,scope,expires_at,created_at,last_used_at,last_used_ip,token_hash FROM personal_access_tokens WHERE token_hash=?1")
        .bind(hash).fetch_optional(pool).await?)
}

pub async fn token_hash(pool: &SqlitePool, token_id: i64, user_id: i64) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT token_hash FROM personal_access_tokens WHERE id=?1 AND user_id=?2",
    )
    .bind(token_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn token_user(pool: &SqlitePool, token_id: i64, write: bool) -> Result<Option<User>> {
    // Missing or restricted OAuth issuers invalidate tokens in every transport, including feeds.
    let issuer = codeyard_db_core::queries::oauth_apps::token_issuer_condition("t", "?3");
    let user: Option<User> = sqlx::query_as(&format!("SELECT u.* FROM users u JOIN personal_access_tokens t ON t.user_id=u.id
        WHERE t.id=?1 AND (t.expires_at IS NULL OR t.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) AND (NOT ?2 OR t.scope='write')
        AND NOT u.is_banned AND NOT u.must_change_password AND {issuer}
        AND NOT EXISTS(SELECT 1 FROM github_app_tokens g WHERE g.token_id=t.id AND NOT EXISTS(
            SELECT 1 FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users owner ON owner.id=a.owner_id
            WHERE i.id=g.installation_id AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password))"))
        .bind(token_id).bind(write).bind(codeyard_model::identity::CLI_CLIENT).fetch_optional(pool).await?;
    if user.as_ref().is_some_and(|user| {
        user.username.ends_with("[bot]")
            && user.username != codeyard_db_core::queries::ci_tokens::BOT
    }) && crate::sqlite::github_apps::token(pool, token_id)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    if user
        .as_ref()
        .is_some_and(|user| user.username == codeyard_db_core::queries::ci_tokens::BOT)
        && codeyard_db_core::queries::sqlite::ci_tokens::get(pool, token_id)
            .await?
            .is_none()
    {
        return Ok(None);
    }
    Ok(user)
}

pub async fn can_write_repository(
    pool: &SqlitePool,
    actor: i64,
    token: i64,
    repo: i64,
    permission: &str,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !codeyard_db_core::queries::sqlite::discussions::lock_write_authority(&mut tx, actor, repo)
        .await?
    {
        return Ok(false);
    }
    let allowed = permits_write(&mut tx, actor, token, Some(repo), None, permission).await?;
    tx.commit().await?;
    Ok(allowed)
}

pub async fn touch_token(pool: &SqlitePool, token_id: i64, address: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE personal_access_tokens SET last_used_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now'),last_used_ip=?2 WHERE id=?1")
        .bind(token_id)
        .bind(address)
        .execute(pool)
        .await?;
    Ok(())
}
