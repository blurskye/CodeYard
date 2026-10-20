use crate::backend::db::deploy_keys::NewDeployKey;
use crate::backend::db::models::DeployKey;
use crate::backend::db::users::ssh_keys::SshKeyWriteRefusal;
use anyhow::Result;
use sqlx::PgPool;

const COLUMNS: &str = "id, repo_id, name AS title, fingerprint, public_key, NOT read_only AS can_write, created_by_id, last_used_at, created_at";

pub async fn list_deploy_keys(pool: &PgPool, repo_id: i64) -> Result<Vec<DeployKey>> {
    let sql =
        format!("SELECT {COLUMNS} FROM deploy_keys WHERE repo_id = $1 ORDER BY created_at, id");
    Ok(sqlx::query_as(&sql).bind(repo_id).fetch_all(pool).await?)
}

pub async fn get_deploy_key(pool: &PgPool, key_id: i64) -> Result<Option<DeployKey>> {
    let sql = format!("SELECT {COLUMNS} FROM deploy_keys WHERE id = $1");
    Ok(sqlx::query_as(&sql)
        .bind(key_id)
        .fetch_optional(pool)
        .await?)
}

pub async fn get_deploy_key_by_fingerprint(
    pool: &PgPool,
    fingerprint: &str,
) -> Result<Option<DeployKey>> {
    let sql = format!("SELECT {COLUMNS} FROM deploy_keys WHERE fingerprint = $1");
    Ok(sqlx::query_as(&sql)
        .bind(fingerprint)
        .fetch_optional(pool)
        .await?)
}

async fn require_admin(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: i64,
    repo: i64,
    session: &str,
) -> Result<()> {
    use super::repositories::access::{lock_user_repo_access_in_transaction, RepositoryRowLock};
    let access =
        lock_user_repo_access_in_transaction(tx, actor, repo, RepositoryRowLock::Shared, true)
            .await?;
    let allowed = access.is_some_and(|access| {
        access.access.can_admin() || (!access.organization && access.is_admin)
    });
    anyhow::ensure!(
        allowed && super::users::transitions::live_session(tx, actor, session).await?,
        crate::backend::repositories::deploy_keys::DeployKeyError::Denied
    );
    Ok(())
}

pub async fn insert_deploy_key(
    pool: &PgPool,
    key: &NewDeployKey,
    session: &str,
) -> Result<DeployKey> {
    let mut tx = pool.begin().await?;
    require_admin(&mut tx, key.created_by_id, key.repo_id, session).await?;
    // The same advisory lock as account-key enrollment, so one fingerprint is checked once.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 37000003))")
        .bind(&key.fingerprint)
        .execute(&mut *tx)
        .await?;
    let used: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ssh_keys WHERE fingerprint = $1) OR EXISTS(SELECT 1 FROM deploy_keys WHERE fingerprint = $1)",
    )
    .bind(&key.fingerprint)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(!used, SshKeyWriteRefusal::AlreadyUsed);
    let sql = format!(
        "INSERT INTO deploy_keys (repo_id, name, fingerprint, public_key, read_only, created_by_id) VALUES ($1, $2, $3, $4, $5, $6) RETURNING {COLUMNS}"
    );
    let row = sqlx::query_as::<_, DeployKey>(&sql)
        .bind(key.repo_id)
        .bind(&key.title)
        .bind(&key.fingerprint)
        .bind(&key.public_key)
        .bind(!key.can_write)
        .bind(key.created_by_id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(row)
}

pub async fn delete_deploy_key(
    pool: &PgPool,
    repo_id: i64,
    key_id: i64,
    actor: i64,
    session: &str,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    require_admin(&mut tx, actor, repo_id, session).await?;
    let result = sqlx::query("DELETE FROM deploy_keys WHERE id = $1 AND repo_id = $2")
        .bind(key_id)
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

/// Records a use. It runs outside any transaction, and its caller ignores a failure.
pub async fn touch_deploy_key(pool: &PgPool, key_id: i64) -> Result<()> {
    sqlx::query("UPDATE deploy_keys SET last_used_at = NOW() WHERE id = $1")
        .bind(key_id)
        .execute(pool)
        .await?;
    Ok(())
}
