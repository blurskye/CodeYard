use crate::backend::db::models::{PersonalAccessToken, User};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// The initiating credential is checked inside the same transaction as its repository write.
/// This only narrows authority; the caller must also hold native repository write authority.
pub(crate) async fn repository_write_on(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    token: i64,
    actor: i64,
    repo: i64,
    permission: &str,
) -> Result<bool> {
    use crate::backend::{db::fine_grained_tokens::Bounds, identity::{fine_grained, github_apps, oauth_apps}};
    let live: Option<String> = sqlx::query_scalar("SELECT u.username FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND t.user_id=$2 AND t.scope='write' AND (t.expires_at IS NULL OR t.expires_at>clock_timestamp()) AND NOT u.is_banned AND NOT u.must_change_password FOR SHARE OF t")
        .bind(token).bind(actor).fetch_optional(&mut **tx).await?;
    let Some(username) = live else { return Ok(false); };
    if username == crate::backend::db::ci_tokens::BOT {
        let job: Option<(i64, String)> = sqlx::query_as("SELECT r.repo_id,t.permissions FROM actions_github_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.token_id=$1 AND t.revoked_at IS NULL AND r.status='running' FOR SHARE OF t,r")
            .bind(token).fetch_optional(&mut **tx).await?;
        return Ok(match job {
            Some((id, permissions)) => id == repo && serde_json::from_str::<crate::backend::ci::tokens::Permissions>(&permissions)?.allows(permission, true),
            None => false,
        });
    }
    let bounds: Option<Bounds> = sqlx::query_as(&format!("{} FOR SHARE OF f,t", super::super::fine_grained_tokens::BOUNDS))
        .bind(token).fetch_optional(&mut **tx).await?;
    if let Some(bounds) = bounds {
        let selected: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r WHERE r.id=$1 AND NOT r.is_gist AND (($2::bigint IS NULL AND r.owner_user_id=$3) OR r.owner_org_id=$2) AND ($4='all' OR ($4='selected' AND EXISTS(SELECT 1 FROM fine_grained_token_repositories s WHERE s.token_id=$5 AND s.repo_id=r.id))))")
            .bind(repo).bind(bounds.owner_org_id).bind(actor).bind(&bounds.repository_access).bind(token).fetch_one(&mut **tx).await?;
        let active = bounds.active();
        let permissions = serde_json::from_str(&bounds.permissions)?;
        return Ok(active && selected && fine_grained::allows(&permissions, &permission.replace('-', "_"), true));
    }
    let bound: Option<(i64, String, String)> = sqlx::query_as("SELECT installation_id,permissions,repositories FROM github_app_tokens WHERE token_id=$1 FOR SHARE")
        .bind(token).fetch_optional(&mut **tx).await?;
    if let Some((installation, permissions, repositories)) = bound {
        let selected: Vec<i64> = serde_json::from_str(&repositories)?;
        let covers: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users u ON u.id=a.owner_id JOIN repositories r ON r.id=$2 WHERE i.id=$1 AND i.suspended_at IS NULL AND NOT u.is_banned AND NOT u.must_change_password AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)))")
            .bind(installation).bind(repo).fetch_one(&mut **tx).await?;
        return Ok(covers && (selected.is_empty() || selected.contains(&repo)) && github_apps::allows(&serde_json::from_str(&permissions)?, &permission.replace('-', "_"), true));
    }
    let oauth: Option<(String, String)> = sqlx::query_as("SELECT client_id,scopes FROM oauth_app_tokens WHERE token_id=$1 FOR SHARE")
        .bind(token).fetch_optional(&mut **tx).await?;
    if let Some((client, scopes)) = oauth {
        let app: Option<(i64, String)> = sqlx::query_as("SELECT a.id,a.permissions FROM github_apps a JOIN users u ON u.id=a.owner_id WHERE a.client_id=$1 AND NOT u.is_banned AND NOT u.must_change_password FOR SHARE OF a")
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

/// Organization team writes retain the initiating credential alongside native organization authority.
pub(crate) async fn team_write_on(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, token: i64, actor: i64, org: i64) -> Result<bool> {
    use crate::backend::{db::fine_grained_tokens::Bounds, identity::{fine_grained, github_apps, oauth_apps}};
    let live: Option<i64> = sqlx::query_scalar("SELECT t.id FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND t.user_id=$2 AND t.scope='write' AND (t.expires_at IS NULL OR t.expires_at>clock_timestamp()) AND NOT u.is_banned AND NOT u.must_change_password AND u.username<>$3 AND NOT EXISTS(SELECT 1 FROM actions_github_tokens j WHERE j.token_id=t.id) FOR SHARE OF t").bind(token).bind(actor).bind(crate::backend::db::ci_tokens::BOT).fetch_optional(&mut **tx).await?;
    if live.is_none() { return Ok(false); }
    let bounds: Option<Bounds> = sqlx::query_as(&format!("{} FOR SHARE OF f,t", super::super::fine_grained_tokens::BOUNDS)).bind(token).fetch_optional(&mut **tx).await?;
    if let Some(bounds) = bounds { return Ok(bounds.active() && bounds.owner_org_id == Some(org) && fine_grained::allows(&serde_json::from_str(&bounds.permissions)?, "members", true)); }
    let bound: Option<(i64, String)> = sqlx::query_as("SELECT installation_id,permissions FROM github_app_tokens WHERE token_id=$1 FOR SHARE").bind(token).fetch_optional(&mut **tx).await?;
    if let Some((installation, permissions)) = bound {
        let covers: Option<i64> = sqlx::query_scalar("SELECT i.id FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users u ON u.id=a.owner_id WHERE i.id=$1 AND i.org_id=$2 AND i.suspended_at IS NULL AND NOT u.is_banned AND NOT u.must_change_password FOR SHARE OF i,a,u").bind(installation).bind(org).fetch_optional(&mut **tx).await?;
        return Ok(covers.is_some() && github_apps::allows(&serde_json::from_str(&permissions)?, "members", true));
    }
    let oauth: Option<(String, String)> = sqlx::query_as("SELECT client_id,scopes FROM oauth_app_tokens WHERE token_id=$1 FOR SHARE").bind(token).fetch_optional(&mut **tx).await?;
    if let Some((client, scopes)) = oauth {
        let app: Option<(i64, String)> = sqlx::query_as("SELECT a.id,a.permissions FROM github_apps a JOIN users u ON u.id=a.owner_id WHERE a.client_id=$1 AND NOT u.is_banned AND NOT u.must_change_password FOR SHARE OF a,u").bind(&client).fetch_optional(&mut **tx).await?;
        if let Some((app, permissions)) = app {
            let covers: Option<i64> = sqlx::query_scalar("SELECT id FROM github_app_installations WHERE app_id=$1 AND org_id=$2 AND suspended_at IS NULL FOR SHARE").bind(app).bind(org).fetch_optional(&mut **tx).await?;
            return Ok(covers.is_some() && github_apps::allows(&serde_json::from_str(&permissions)?, "members", true));
        }
        let client_exists = client == oauth_apps::CLI_CLIENT || sqlx::query_scalar::<_, i64>("SELECT id FROM oauth_applications WHERE client_id=$1 FOR SHARE").bind(client).fetch_optional(&mut **tx).await?.is_some();
        return Ok(client_exists && oauth_apps::has_scope(&scopes, "write:org"));
    }
    let policy: Option<bool> = sqlx::query_scalar("SELECT classic_allowed FROM organization_token_policies WHERE org_id=$1 FOR SHARE").bind(org).fetch_optional(&mut **tx).await?;
    Ok(policy.unwrap_or(true))
}

pub async fn repository_write(pool: &PgPool, token: i64, actor: i64, repo: i64, permission: &str) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let allowed = repository_write_on(&mut tx, token, actor, repo, permission).await?;
    tx.commit().await?;
    Ok(allowed)
}

pub async fn create_token(
    pool: &PgPool,
    user_id: i64,
    session_id: &str,
    name: &str,
    hash: &str,
    scope: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Result<Option<PersonalAccessToken>> {
    anyhow::ensure!(crate::backend::db::users::tokens::supported_expiry(expires_at), "Token expiry is outside the supported calendar range");
    let mut tx = pool.begin().await?;
    if !super::transitions::active_session(&mut tx, user_id, session_id).await? {
        return Ok(None);
    }
    let token = sqlx::query_as("INSERT INTO personal_access_tokens(user_id,name,token_hash,scope,expires_at) VALUES($1,$2,$3,$4,$5) RETURNING id,user_id,name,scope,expires_at,created_at")
        .bind(user_id).bind(name).bind(hash).bind(scope).bind(expires_at).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(token))
}

/// The newest 100 tokens older than `before`, or the newest 100 when it is 0.
pub async fn list_tokens(
    pool: &PgPool,
    user_id: i64,
    session_id: &str,
    before: i64,
) -> Result<Vec<PersonalAccessToken>> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(super::transitions::active_session_read(&mut tx, user_id, session_id).await?, crate::backend::db::users::accounts::AccountRestricted);
    let tokens = sqlx::query_as("SELECT id,user_id,name,scope,expires_at,created_at,last_used_at,last_used_ip FROM personal_access_tokens WHERE user_id=$1 AND ($2<=0 OR id<$2) AND NOT EXISTS(SELECT 1 FROM fine_grained_tokens f WHERE f.token_id=personal_access_tokens.id) ORDER BY id DESC LIMIT 100")
        .bind(user_id).bind(before).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(tokens)
}

pub async fn delete_token(
    pool: &PgPool,
    user_id: i64,
    session_id: &str,
    token_id: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    if !super::transitions::active_session(&mut tx, user_id, session_id).await? { return Ok(false); }
    let deleted = sqlx::query("DELETE FROM personal_access_tokens WHERE id=$1 AND user_id=$2")
        .bind(token_id).bind(user_id).execute(&mut *tx).await?.rows_affected() == 1;
    tx.commit().await?;
    Ok(deleted)
}

// Include expired and suspended tokens here so failures share the account's login budget.
pub async fn find_token(pool: &PgPool, hash: &str) -> Result<Option<PersonalAccessToken>> {
    Ok(sqlx::query_as("SELECT id,user_id,name,scope,expires_at,created_at,last_used_at,last_used_ip,token_hash FROM personal_access_tokens WHERE token_hash=$1")
        .bind(hash).fetch_optional(pool).await?)
}

pub async fn token_hash(pool: &PgPool, token_id: i64, user_id: i64) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT token_hash FROM personal_access_tokens WHERE id=$1 AND user_id=$2")
        .bind(token_id).bind(user_id).fetch_optional(pool).await?)
}

pub async fn token_user(pool: &PgPool, token_id: i64, write: bool) -> Result<Option<User>> {
    // Missing or restricted OAuth issuers invalidate tokens in every transport, including feeds.
    let issuer = crate::backend::db::oauth_apps::token_issuer_condition("t", "$3");
    let user: Option<User> = sqlx::query_as(&format!("SELECT u.* FROM users u JOIN personal_access_tokens t ON t.user_id=u.id
        WHERE t.id=$1 AND (t.expires_at IS NULL OR t.expires_at>clock_timestamp()) AND (NOT $2 OR t.scope='write')
        AND NOT u.is_banned AND NOT u.must_change_password AND {issuer}
        AND NOT EXISTS(SELECT 1 FROM github_app_tokens g WHERE g.token_id=t.id AND NOT EXISTS(
            SELECT 1 FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users owner ON owner.id=a.owner_id
            WHERE i.id=g.installation_id AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password))"))
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

pub async fn can_write_repository(pool: &PgPool, actor: i64, token: i64, repo: i64, permission: &str) -> Result<bool> {
    let mut tx = pool.begin().await?;
    if !super::super::discussions::lock_write_authority(&mut tx, actor, repo).await? {
        return Ok(false);
    }
    let allowed = permits_write(&mut tx, actor, token, Some(repo), None, permission).await?;
    tx.commit().await?;
    Ok(allowed)
}

/// Called after resource authority has been locked, in the transaction that writes it.
/// The token row lock serializes revocation with the effect, not just HTTP authentication.
pub(crate) async fn permits_write(
    tx: &mut sqlx::PgConnection,
    actor: i64,
    token: i64,
    repo: Option<i64>,
    org: Option<i64>,
    permission: &str,
) -> Result<bool> {
    super::transitions::lock_membership_shared(tx).await?;
    let live: Option<String> = sqlx::query_scalar("SELECT u.username FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND u.id=$2 AND t.scope='write' AND (t.expires_at IS NULL OR t.expires_at>clock_timestamp()) AND NOT u.is_banned AND NOT u.must_change_password FOR SHARE OF t")
        .bind(token).bind(actor).fetch_optional(&mut *tx).await?;
    let Some(username) = live else { return Ok(false) };
    let repository: Option<crate::backend::db::models::Repository> = match repo {
        Some(repo) => sqlx::query_as("SELECT * FROM repositories WHERE id=$1").bind(repo).fetch_optional(&mut *tx).await?,
        None => None,
    };
    let org = org.or_else(|| repository.as_ref().and_then(|r| r.owner_org_id));
    let bounds: Option<crate::backend::db::fine_grained_tokens::Bounds> = sqlx::query_as(&format!("{} FOR SHARE OF f", super::super::fine_grained_tokens::BOUNDS))
        .bind(token).fetch_optional(&mut *tx).await?;
    if let Some(bounds) = bounds {
        let active = bounds.active();
        let permissions: serde_json::Value = serde_json::from_str(&bounds.permissions)?;
        let reaches = if let Some(repository) = &repository {
            let selected: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM fine_grained_token_repositories WHERE token_id=$1 AND repo_id=$2)")
                .bind(token).bind(repository.id).fetch_one(&mut *tx).await?;
            (bounds.owner_org_id.is_some() && repository.owner_org_id == bounds.owner_org_id
                || bounds.owner_org_id.is_none() && repository.owner_user_id == Some(actor))
                && (bounds.repository_access == "all" || bounds.repository_access == "selected" && selected)
        } else {
            org.is_some() && bounds.owner_org_id == org
        };
        return Ok(active && reaches && crate::backend::identity::github_apps::allows(&permissions, permission, true));
    }
    let app_token: Option<crate::backend::db::github_apps::Token> = sqlx::query_as("SELECT * FROM github_app_tokens WHERE token_id=$1 FOR SHARE")
        .bind(token).fetch_optional(&mut *tx).await?;
    let oauth: Option<(String, String, bool)> = sqlx::query_as("SELECT o.client_id,o.scopes,COALESCE(c.is_github_app,FALSE) FROM oauth_app_tokens o LEFT JOIN oauth_applications c ON c.client_id=o.client_id WHERE o.token_id=$1")
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
        let grant: Option<String> = sqlx::query_scalar("SELECT a.permissions FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id JOIN repositories r ON r.id=$1 WHERE ($2::BIGINT IS NULL OR i.id=$2) AND ($2::BIGINT IS NOT NULL OR a.client_id=$3) AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)) FOR SHARE OF i,a")
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
        let job: Option<(i64, String)> = sqlx::query_as("SELECT r.repo_id,t.permissions FROM actions_github_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.token_id=$1 AND t.revoked_at IS NULL AND r.status='running' FOR SHARE OF t,r")
            .bind(token).fetch_optional(&mut *tx).await?;
        return match job {
            Some((id, permissions)) => Ok(repo == Some(id) && serde_json::from_str::<crate::backend::ci::tokens::Permissions>(&permissions)?.allows(&permission.replace('_', "-"), true)),
            None => Ok(false),
        };
    }
    if let Some((client, scopes, _)) = oauth {
        let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oauth_applications c JOIN users owner ON owner.id=c.owner_id WHERE c.client_id=$1 AND NOT owner.is_banned AND NOT owner.must_change_password)")
            .bind(&client).fetch_one(&mut *tx).await?;
        let allowed = crate::backend::identity::oauth_apps::has_scope(&scopes, if repo.is_some() { "repo" } else { "write:org" })
            || permission == "security_events" && crate::backend::identity::oauth_apps::has_scope(&scopes, "security_events");
        return Ok((active || client == crate::backend::identity::oauth_apps::CLI_CLIENT) && allowed);
    }
    let refused: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_token_policies WHERE org_id=$1 AND NOT classic_allowed)")
        .bind(org).fetch_one(&mut *tx).await?;
    Ok(!refused)
}

pub async fn touch_token(pool: &PgPool, token_id: i64, address: Option<&str>) -> Result<()> {
    sqlx::query(
        "UPDATE personal_access_tokens SET last_used_at=clock_timestamp(),last_used_ip=$2 WHERE id=$1",
    )
    .bind(token_id)
    .bind(address)
    .execute(pool)
    .await?;
    Ok(())
}
