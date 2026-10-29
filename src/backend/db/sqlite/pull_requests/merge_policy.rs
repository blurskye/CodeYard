use crate::backend::db::pull_requests::merge_policy::{Changed, Lease, Snapshot};
use anyhow::{ensure, Result};
use sqlx::SqlitePool;

pub async fn revision(pool: &SqlitePool, repo: i64) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COALESCE((SELECT version FROM repository_merge_epochs WHERE repo_id=?1),0)")
        .bind(repo).fetch_one(pool).await?)
}

pub async fn retain(pool: &SqlitePool, repo: i64, expected: &Snapshot) -> Result<Lease> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let current: Option<i64> = sqlx::query_scalar("SELECT COALESCE((SELECT version FROM repository_merge_epochs WHERE repo_id=r.id),0) FROM repositories r WHERE r.id=?1")
        .bind(repo).fetch_optional(&mut *tx).await?;
    ensure!(current == Some(expected.revision), Changed);
    if let Some(consent) = &expected.auto_merge {
        let current: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT a.enabled_at FROM pull_request_auto_merges a JOIN pull_requests p ON p.id=a.pull_request_id WHERE p.repo_id=?1 AND a.pull_request_id=?2")
            .bind(repo).bind(consent.pull_request_id).fetch_optional(&mut *tx).await?;
        ensure!(current == Some(consent.enabled_at), Changed);
    }
    Ok(Lease::Sqlite(tx))
}
