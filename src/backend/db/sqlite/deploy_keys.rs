use crate::backend::db::deploy_keys::NewDeployKey;
use crate::backend::db::models::DeployKey;
use crate::backend::db::users::ssh_keys::SshKeyWriteRefusal;
use anyhow::Result;
use sqlx::SqlitePool;

const COLUMNS: &str = "id, repo_id, name AS title, fingerprint, public_key, NOT read_only AS can_write, created_by_id, last_used_at, created_at";

pub async fn list_deploy_keys(pool: &SqlitePool, repo_id: i64) -> Result<Vec<DeployKey>> {
    let sql =
        format!("SELECT {COLUMNS} FROM deploy_keys WHERE repo_id = ?1 ORDER BY created_at, id");
    Ok(sqlx::query_as(&sql).bind(repo_id).fetch_all(pool).await?)
}

pub async fn get_deploy_key(pool: &SqlitePool, key_id: i64) -> Result<Option<DeployKey>> {
    let sql = format!("SELECT {COLUMNS} FROM deploy_keys WHERE id = ?1");
    Ok(sqlx::query_as(&sql)
        .bind(key_id)
        .fetch_optional(pool)
        .await?)
}

pub async fn get_deploy_key_by_fingerprint(
    pool: &SqlitePool,
    fingerprint: &str,
) -> Result<Option<DeployKey>> {
    let sql = format!("SELECT {COLUMNS} FROM deploy_keys WHERE fingerprint = ?1");
    Ok(sqlx::query_as(&sql)
        .bind(fingerprint)
        .fetch_optional(pool)
        .await?)
}

pub async fn insert_deploy_key(pool: &SqlitePool, key: &NewDeployKey) -> Result<DeployKey> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let used: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ssh_keys WHERE fingerprint = ?1) OR EXISTS(SELECT 1 FROM deploy_keys WHERE fingerprint = ?1)",
    )
    .bind(&key.fingerprint)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(!used, SshKeyWriteRefusal::AlreadyUsed);
    let sql = format!(
        "INSERT INTO deploy_keys (repo_id, name, fingerprint, public_key, read_only, created_by_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING {COLUMNS}"
    );
    let row = sqlx::query_as::<_, DeployKey>(&sql)
        .bind(key.repo_id)
        .bind(&key.title)
        .bind(&key.fingerprint)
        .bind(&key.public_key)
        .bind(!key.can_write)
        .bind(key.created_by_id)
        .fetch_one(&mut *tx)
        .await
        // The unique index is the final guard for a concurrent insert of the same key.
        .map_err(|error| match &error {
            sqlx::Error::Database(database) if database.is_unique_violation() => {
                anyhow::Error::from(SshKeyWriteRefusal::AlreadyUsed)
            }
            _ => anyhow::Error::from(error),
        })?;
    tx.commit().await?;
    Ok(row)
}

pub async fn delete_deploy_key(pool: &SqlitePool, repo_id: i64, key_id: i64) -> Result<bool> {
    let result = sqlx::query("DELETE FROM deploy_keys WHERE id = ?1 AND repo_id = ?2")
        .bind(key_id)
        .bind(repo_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Records a use. It runs outside any transaction, and its caller ignores a failure.
pub async fn touch_deploy_key(pool: &SqlitePool, key_id: i64) -> Result<()> {
    sqlx::query("UPDATE deploy_keys SET last_used_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id = ?1")
        .bind(key_id)
        .execute(pool)
        .await?;
    Ok(())
}
