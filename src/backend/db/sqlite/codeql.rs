use crate::backend::db::code_scanning::Refusal;
use crate::backend::db::codeql::*;
use crate::backend::db::credentials::Actor;
use anyhow::{ensure, Result};
use sqlx::SqlitePool;

async fn authorized(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: &Actor,
    repo: i64,
    write: bool,
) -> Result<()> {
    let allowed = if write {
        crate::backend::db::sqlite::discussions::lock_write_authority(tx, actor.user_id, repo)
            .await?
    } else {
        crate::backend::db::sqlite::repositories::access::lock_readable_repository_in_transaction(
            tx,
            actor.user_id,
            repo,
        )
        .await?
    };
    ensure!(
        allowed
            && crate::backend::db::sqlite::credentials::repository(
                tx,
                actor,
                repo,
                "security_events",
                write
            )
            .await?,
        Refusal::Denied
    );
    Ok(())
}

pub async fn list(pool: &SqlitePool, actor: impl Into<Actor>, repo: i64) -> Result<Vec<Database>> {
    let actor = actor.into();
    let mut tx = pool.begin().await?;
    authorized(&mut tx, &actor, repo, false).await?;
    let rows = sqlx::query_as("SELECT * FROM codeql_databases WHERE repo_id=$1 ORDER BY language")
        .bind(repo)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn get(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    repo: i64,
    language: &str,
) -> Result<Option<Database>> {
    let actor = actor.into();
    let mut tx = pool.begin().await?;
    authorized(&mut tx, &actor, repo, false).await?;
    let row = sqlx::query_as("SELECT * FROM codeql_databases WHERE repo_id=$1 AND language=$2")
        .bind(repo)
        .bind(language)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(row)
}

pub async fn stage(pool: &SqlitePool, actor: impl Into<Actor>, repo: i64, key: &str) -> Result<()> {
    let actor = actor.into();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorized(&mut tx, &actor, repo, true).await?;
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
    authorized(&mut tx, &input.actor, input.repo, true).await?;
    let pending =
        sqlx::query("DELETE FROM pending_object_deletions WHERE object_key=$1 AND delete_after>$2")
            .bind(input.key)
            .bind(chrono::Utc::now())
            .execute(&mut *tx)
            .await?;
    ensure!(pending.rows_affected() == 1, Refusal::Missing);
    let confirmed = sqlx::query("UPDATE upload_rate_limit SET confirmed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 AND user_id=?2 AND file_size=?3 AND reason=?4 AND file_id IS NULL AND confirmed_at IS NULL")
        .bind(input.charge).bind(input.actor.user_id).bind(input.size).bind(UPLOAD_QUOTA).execute(&mut *tx).await?;
    ensure!(confirmed.rows_affected() == 1, Refusal::Missing);
    let row = sqlx::query_as("INSERT INTO codeql_databases(repo_id,language,name,uploader_id,object_key,size,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$6,$7,$7) ON CONFLICT(repo_id,language) DO UPDATE SET name=excluded.name,uploader_id=excluded.uploader_id,object_key=excluded.object_key,size=excluded.size,updated_at=excluded.updated_at RETURNING *")
        .bind(input.repo).bind(input.language).bind(input.name).bind(input.actor.user_id).bind(input.key).bind(input.size).bind(chrono::Utc::now()).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}

pub async fn delete(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    repo: i64,
    language: &str,
) -> Result<()> {
    let actor = actor.into();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorized(&mut tx, &actor, repo, true).await?;
    let removed = sqlx::query("DELETE FROM codeql_databases WHERE repo_id=$1 AND language=$2")
        .bind(repo)
        .bind(language)
        .execute(&mut *tx)
        .await?;
    ensure!(removed.rows_affected() == 1, Refusal::Missing);
    tx.commit().await?;
    Ok(())
}
