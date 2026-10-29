use crate::backend::db::{code_scanning::Refusal, codeql::*};
use anyhow::{ensure, Result};
use sqlx::SqlitePool;

pub async fn list(pool: &SqlitePool, repo: i64) -> Result<Vec<Database>> {
    Ok(
        sqlx::query_as("SELECT * FROM codeql_databases WHERE repo_id=$1 ORDER BY language")
            .bind(repo)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn get(pool: &SqlitePool, repo: i64, language: &str) -> Result<Option<Database>> {
    Ok(
        sqlx::query_as("SELECT * FROM codeql_databases WHERE repo_id=$1 AND language=$2")
            .bind(repo)
            .bind(language)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn stage(pool: &SqlitePool, actor: i64, repo: i64, key: &str) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        super::discussions::lock_write_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    // A crashed or refused upload is reclaimed by the existing object sweep.
    sqlx::query("INSERT INTO pending_object_deletions(object_key,delete_after) VALUES($1,$2)")
        .bind(key)
        .bind(chrono::Utc::now() + chrono::Duration::hours(1))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn publish(pool: &SqlitePool, input: &NewDatabase<'_>) -> Result<Database> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        super::discussions::lock_write_authority(&mut tx, input.actor, input.repo).await?,
        Refusal::Denied
    );
    let pending =
        sqlx::query("DELETE FROM pending_object_deletions WHERE object_key=$1 AND delete_after>$2")
            .bind(input.key)
            .bind(chrono::Utc::now())
            .execute(&mut *tx)
            .await?;
    ensure!(pending.rows_affected() == 1, Refusal::Missing);
    let confirmed = sqlx::query("UPDATE upload_rate_limit SET confirmed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 AND user_id=?2 AND file_size=?3 AND reason=?4 AND file_id IS NULL AND confirmed_at IS NULL")
        .bind(input.charge).bind(input.actor).bind(input.size).bind(UPLOAD_QUOTA).execute(&mut *tx).await?;
    ensure!(confirmed.rows_affected() == 1, Refusal::Missing);
    let row = sqlx::query_as("INSERT INTO codeql_databases(repo_id,language,name,uploader_id,object_key,size,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$6,$7,$7) ON CONFLICT(repo_id,language) DO UPDATE SET name=excluded.name,uploader_id=excluded.uploader_id,object_key=excluded.object_key,size=excluded.size,updated_at=excluded.updated_at RETURNING *")
        .bind(input.repo).bind(input.language).bind(input.name).bind(input.actor).bind(input.key).bind(input.size).bind(chrono::Utc::now()).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}

pub async fn delete(pool: &SqlitePool, actor: i64, repo: i64, language: &str) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        super::discussions::lock_write_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    let removed = sqlx::query("DELETE FROM codeql_databases WHERE repo_id=$1 AND language=$2")
        .bind(repo)
        .bind(language)
        .execute(&mut *tx)
        .await?;
    ensure!(removed.rows_affected() == 1, Refusal::Missing);
    tx.commit().await?;
    Ok(())
}
