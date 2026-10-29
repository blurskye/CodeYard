use crate::backend::db::{
    fine_grained_tokens::{Bounds, NewToken, Policy, Token},
    models::OrgRole,
    organizations::OrganizationOutcome,
    sqlite::time,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::{SqliteConnection, SqlitePool};

/// A token's bounds with its live membership and policy; package transactions read it too.
pub(crate) const BOUNDS: &str = "SELECT f.token_id,t.user_id,f.owner_org_id,f.repository_access,f.permissions,f.status,t.expires_at,f.requested_at,(f.owner_org_id IS NULL OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=f.owner_org_id AND m.user_id=t.user_id)) AS member,COALESCE(p.fine_grained_allowed,1) AS fine_grained_allowed,CASE WHEN p.org_id IS NULL THEN 366 ELSE p.max_lifetime_days END AS max_lifetime_days FROM fine_grained_tokens f JOIN personal_access_tokens t ON t.id=f.token_id LEFT JOIN organization_token_policies p ON p.org_id=f.owner_org_id WHERE f.token_id=$1";
const TOKEN: &str = "SELECT f.token_id,t.user_id,t.name,f.description,f.owner_org_id,f.repository_access,f.permissions,f.status,f.reason,f.reviewed_at,f.requested_at,t.expires_at,t.created_at,t.last_used_at,t.last_used_ip FROM fine_grained_tokens f JOIN personal_access_tokens t ON t.id=f.token_id";
const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%f000Z','now')";

pub async fn create(
    pool: &SqlitePool,
    user: i64,
    session: &str,
    hash: &str,
    token: &NewToken<'_>,
) -> Result<Option<(i64, String)>> {
    // The write lock serializes with suspension, password resets and membership changes.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let live: bool = sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM users u JOIN sessions s ON s.user_id=u.id WHERE u.id=$1 AND s.id=$2 AND s.expires_at>{NOW} AND NOT u.is_banned AND NOT u.must_change_password)"))
        .bind(user).bind(session).fetch_one(&mut *tx).await?;
    if !live {
        return Ok(None);
    }
    let status = match token.owner_org_id {
        None => "approved",
        Some(org) => {
            let row: Option<(String, bool, bool)> = sqlx::query_as("SELECT m.role,COALESCE(p.fine_grained_allowed,1),COALESCE(p.fine_grained_approval,1) FROM org_memberships m LEFT JOIN organization_token_policies p ON p.org_id=m.org_id WHERE m.org_id=$1 AND m.user_id=$2")
                .bind(org).bind(user).fetch_optional(&mut *tx).await?;
            match row {
                Some((_, false, _)) | None => return Ok(None),
                Some((role, true, approval)) if approval && role != "owner" => "pending",
                Some(_) => "approved",
            }
        }
    };
    let id: i64 = sqlx::query_scalar("INSERT INTO personal_access_tokens(user_id,name,token_hash,scope,expires_at) VALUES($1,$2,$3,$4,$5) RETURNING id")
        .bind(user).bind(token.name).bind(hash).bind(token.scope).bind(token.expires_at.map(time::encode)).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO fine_grained_tokens(token_id,owner_org_id,description,repository_access,permissions,status) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(id).bind(token.owner_org_id).bind(token.description).bind(token.repository_access).bind(token.permissions).bind(status).execute(&mut *tx).await?;
    for repo in token.repositories {
        // Only the owner's own repositories can be selected.
        sqlx::query("INSERT OR IGNORE INTO fine_grained_token_repositories(token_id,repo_id) SELECT $1,id FROM repositories WHERE id=$2 AND NOT is_gist AND (($3 IS NULL AND owner_kind='user' AND owner_user_id=$4) OR (owner_kind='org' AND owner_org_id=$3))")
            .bind(id).bind(repo).bind(token.owner_org_id).bind(user).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Some((id, status.to_owned())))
}

pub async fn regenerate(
    pool: &SqlitePool,
    user: i64,
    session: &str,
    token: i64,
    hash: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let changed = sqlx::query(&format!("UPDATE personal_access_tokens SET token_hash=$4,expires_at=$5,last_used_at=NULL,last_used_ip=NULL WHERE id=$1 AND user_id=$2 AND EXISTS(SELECT 1 FROM fine_grained_tokens WHERE token_id=$1) AND EXISTS(SELECT 1 FROM users WHERE id=$2 AND NOT is_banned AND NOT must_change_password) AND EXISTS(SELECT 1 FROM sessions WHERE id=$3 AND user_id=$2 AND expires_at>{NOW})"))
        .bind(token).bind(user).bind(session).bind(hash).bind(expires_at.map(time::encode)).execute(&mut *tx).await?.rows_affected() == 1;
    if changed {
        sqlx::query(&format!(
            "UPDATE fine_grained_tokens SET requested_at={NOW} WHERE token_id=$1"
        ))
        .bind(token)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

pub async fn get(pool: &SqlitePool, token: i64) -> Result<Option<Token>> {
    Ok(sqlx::query_as(&format!("{TOKEN} WHERE f.token_id=$1"))
        .bind(token)
        .fetch_optional(pool)
        .await?)
}

pub async fn list(pool: &SqlitePool, user: i64) -> Result<Vec<Token>> {
    list_before(pool, user, None).await
}

pub async fn list_before(pool: &SqlitePool, user: i64, before: Option<i64>) -> Result<Vec<Token>> {
    Ok(sqlx::query_as(&format!("{TOKEN} WHERE t.user_id=$1 AND (CAST($2 AS BIGINT) IS NULL OR f.token_id<$2) ORDER BY f.token_id DESC LIMIT 100"))
        .bind(user).bind(before).fetch_all(pool).await?)
}

pub async fn bounds(pool: &SqlitePool, token: i64) -> Result<Option<Bounds>> {
    Ok(sqlx::query_as(BOUNDS)
        .bind(token)
        .fetch_optional(pool)
        .await?)
}

pub async fn repositories(pool: &SqlitePool, token: i64) -> Result<Vec<(i64, String)>> {
    Ok(sqlx::query_as(
        "SELECT r.id,r.name FROM fine_grained_token_repositories f JOIN repositories r ON r.id=f.repo_id WHERE f.token_id=$1 ORDER BY r.name,r.id LIMIT 100",
    )
    .bind(token)
    .fetch_all(pool)
    .await?)
}

pub async fn selected(pool: &SqlitePool, token: i64, repo: i64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM fine_grained_token_repositories WHERE token_id=$1 AND repo_id=$2)",
    )
    .bind(token)
    .bind(repo)
    .fetch_one(pool)
    .await?)
}

pub async fn policy(pool: &SqlitePool, org: i64) -> Result<Policy> {
    Ok(sqlx::query_as("SELECT fine_grained_allowed,fine_grained_approval,classic_allowed,max_lifetime_days FROM organization_token_policies WHERE org_id=$1")
        .bind(org).fetch_optional(pool).await?.unwrap_or_default())
}

/// Policy and review writes need an active owner of an organization that still has one,
/// checked by the shared organization guard inside the writing transaction.
async fn active_owner(connection: &mut SqliteConnection, actor: i64, org: i64) -> Result<bool> {
    Ok(matches!(
        super::organizations::authority::authority(connection, actor, org).await?,
        OrganizationOutcome::Applied(OrgRole::Owner)
    ))
}

pub async fn set_policy(pool: &SqlitePool, actor: i64, org: i64, policy: &Policy) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !active_owner(&mut tx, actor, org).await? {
        return Ok(false);
    }
    sqlx::query("INSERT INTO organization_token_policies(org_id,fine_grained_allowed,fine_grained_approval,classic_allowed,max_lifetime_days) VALUES($1,$2,$3,$4,$5) ON CONFLICT(org_id) DO UPDATE SET fine_grained_allowed=excluded.fine_grained_allowed,fine_grained_approval=excluded.fine_grained_approval,classic_allowed=excluded.classic_allowed,max_lifetime_days=excluded.max_lifetime_days")
        .bind(org).bind(policy.fine_grained_allowed).bind(policy.fine_grained_approval).bind(policy.classic_allowed).bind(policy.max_lifetime_days).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn organization_tokens(pool: &SqlitePool, org: i64, status: &str) -> Result<Vec<Token>> {
    Ok(sqlx::query_as(&format!("{TOKEN} WHERE f.owner_org_id=$1 AND f.status=$2 AND EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=$1 AND m.user_id=t.user_id) ORDER BY f.token_id DESC LIMIT 1000"))
        .bind(org).bind(status).fetch_all(pool).await?)
}

pub async fn review(
    pool: &SqlitePool,
    org: i64,
    reviewer: i64,
    tokens: &[i64],
    from: &str,
    status: &str,
    reason: Option<&str>,
) -> Result<u64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !active_owner(&mut tx, reviewer, org).await? {
        return Ok(0);
    }
    let mut changed = 0;
    for token in tokens {
        changed += sqlx::query(&format!("UPDATE fine_grained_tokens SET status=$4,reason=$5,reviewed_by=$3,reviewed_at={NOW} WHERE token_id=$1 AND owner_org_id=$2 AND status=$6"))
            .bind(token).bind(org).bind(reviewer).bind(status).bind(reason).bind(from).execute(&mut *tx).await?.rows_affected();
    }
    tx.commit().await?;
    Ok(changed)
}

pub async fn classic_refused(pool: &SqlitePool, token: i64, org: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_token_policies p WHERE p.org_id=$2 AND NOT p.classic_allowed) AND NOT EXISTS(SELECT 1 FROM oauth_app_tokens WHERE token_id=$1) AND NOT EXISTS(SELECT 1 FROM github_app_tokens WHERE token_id=$1) AND NOT EXISTS(SELECT 1 FROM fine_grained_tokens WHERE token_id=$1) AND NOT EXISTS(SELECT 1 FROM actions_github_tokens WHERE token_id=$1)")
        .bind(token).bind(org).fetch_one(pool).await?)
}

pub async fn classic_refused_repository(pool: &SqlitePool, token: i64, repo: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r JOIN organization_token_policies p ON p.org_id=r.owner_org_id WHERE r.id=$2 AND NOT p.classic_allowed) AND NOT EXISTS(SELECT 1 FROM oauth_app_tokens WHERE token_id=$1) AND NOT EXISTS(SELECT 1 FROM github_app_tokens WHERE token_id=$1) AND NOT EXISTS(SELECT 1 FROM fine_grained_tokens WHERE token_id=$1) AND NOT EXISTS(SELECT 1 FROM actions_github_tokens WHERE token_id=$1)")
        .bind(token).bind(repo).fetch_one(pool).await?)
}
