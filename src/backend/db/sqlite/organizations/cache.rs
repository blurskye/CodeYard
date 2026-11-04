use crate::backend::db::action_objects::OrganizationCacheUsage;
use crate::backend::db::actions::CacheLimit;
use crate::backend::db::actions::CachePolicyError;
use crate::backend::db::actions::OrgCacheSettings;
use crate::backend::db::sqlite::organizations::authority::begin_read;

use crate::backend::db::organizations::OrganizationOutcome;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn get_cache_settings(
    pool: &SqlitePool,
    actor: i64,
    name: &str,
) -> Result<OrganizationOutcome<OrgCacheSettings>> {
    let mut tx = begin_read(pool).await?;
    let org: Option<i64> = sqlx::query_scalar("SELECT id FROM organizations WHERE name=$1")
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(org) = org else {
        return Ok(OrganizationOutcome::Missing);
    };
    let role = require_read_org!(tx, actor, org);
    if !crate::backend::db::organizations::administers(role) {
        return Ok(OrganizationOutcome::Denied);
    }
    let settings = sqlx::query_as(
        "SELECT cache_size_gib,cache_retention_days FROM org_actions_settings WHERE org_id=$1",
    )
    .bind(org)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or_default();
    tx.commit().await?;
    Ok(OrganizationOutcome::Applied(settings))
}

pub async fn set_cache_limit(
    pool: &SqlitePool,
    actor: i64,
    name: &str,
    kind: CacheLimit,
    value: i32,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let org: Option<i64> = sqlx::query_scalar("SELECT id FROM organizations WHERE name=$1")
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(org) = org else {
        return Ok(OrganizationOutcome::Missing);
    };
    let role = require_org!(tx, actor, org);
    if !crate::backend::db::organizations::administers(role) {
        return Ok(OrganizationOutcome::Denied);
    }
    let column = kind.column();
    let maximum: i32 = sqlx::query_scalar(&format!(
        "SELECT COALESCE({column},{}) FROM actions_settings WHERE id=1",
        kind.default_value()
    ))
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(kind.default_value());
    anyhow::ensure!(value > 0 && value <= maximum, CachePolicyError::Limit);
    let repos: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM repositories WHERE owner_org_id=$1 AND owner_kind='org' ORDER BY id",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query(&format!("INSERT INTO org_actions_settings(org_id,{column}) VALUES($1,$2) ON CONFLICT(org_id) DO UPDATE SET {column}=EXCLUDED.{column},updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now')")).bind(org).bind(value).execute(&mut *tx).await?;
    for repo in repos {
        let (bytes, days) =
            crate::backend::db::sqlite::action_objects::cache_policy(&mut tx, repo).await?;
        match kind {
            CacheLimit::Storage => {
                crate::backend::db::sqlite::action_objects::enforce_cache_limit(
                    &mut tx, repo, bytes,
                )
                .await?
            }
            CacheLimit::Retention => {
                sqlx::query("UPDATE actions_objects SET expires_at=MIN(expires_at,accessed_at+$2) WHERE repo_id=$1 AND kind='cache'").bind(repo).bind(i64::from(days)*86400).execute(&mut *tx).await?;
            }
        }
    }
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,'organization.actions_cache_policy','organization',$2,$3)").bind(actor).bind(org).bind(serde_json::json!({"setting":column,"value":value})).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(OrganizationOutcome::Applied(()))
}

pub async fn org_cache_usage(
    pool: &SqlitePool,
    actor: i64,
    name: &str,
    offset: i64,
    limit: i64,
) -> Result<OrganizationOutcome<OrganizationCacheUsage>> {
    anyhow::ensure!(
        offset >= 0 && (0..=100).contains(&limit),
        "invalid cache usage page"
    );
    let mut tx = begin_read(pool).await?;
    let org: Option<i64> = sqlx::query_scalar("SELECT id FROM organizations WHERE name=$1")
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(org) = org else {
        return Ok(OrganizationOutcome::Missing);
    };
    let role = require_read_org!(tx, actor, org);
    if !crate::backend::db::organizations::administers(role) {
        return Ok(OrganizationOutcome::Denied);
    }
    let (count,bytes)=sqlx::query_as("SELECT COUNT(*),COALESCE(SUM(o.size),0) FROM actions_objects o JOIN repositories r ON r.id=o.repo_id WHERE r.owner_org_id=$1 AND r.owner_kind='org' AND o.kind='cache' AND o.finalized AND o.expires_at>CAST(strftime('%s','now') AS INTEGER)").bind(org).fetch_one(&mut *tx).await?;
    let total = sqlx::query_scalar(
        "SELECT COUNT(*) FROM repositories WHERE owner_org_id=$1 AND owner_kind='org'",
    )
    .bind(org)
    .fetch_one(&mut *tx)
    .await?;
    let repositories=sqlx::query_as("SELECT r.name,COUNT(o.id) AS count,COALESCE(SUM(o.size),0) AS bytes FROM repositories r LEFT JOIN actions_objects o ON o.repo_id=r.id AND o.kind='cache' AND o.finalized AND o.expires_at>CAST(strftime('%s','now') AS INTEGER) WHERE r.owner_org_id=$1 AND r.owner_kind='org' GROUP BY r.id,r.name ORDER BY r.id LIMIT $2 OFFSET $3").bind(org).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(OrganizationOutcome::Applied(OrganizationCacheUsage {
        total,
        count,
        bytes,
        repositories,
    }))
}
