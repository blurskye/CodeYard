use crate::backend::db::pull_requests::merge_policy::{Changed, Snapshot};
use anyhow::{ensure, Result};
use sqlx::SqlitePool;

pub async fn revision(pool: &SqlitePool, repo: i64) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COALESCE((SELECT version FROM repository_merge_epochs WHERE repo_id=?1),0)")
        .bind(repo).fetch_one(pool).await?)
}

pub async fn check(pool: &SqlitePool, repo: i64, expected: &Snapshot) -> Result<()> {
    let current: Option<i64> = sqlx::query_scalar("SELECT COALESCE((SELECT version FROM repository_merge_epochs WHERE repo_id=r.id),0) FROM repositories r WHERE r.id=?1")
        .bind(repo).fetch_optional(pool).await?;
    ensure!(current == Some(expected.revision), Changed);
    if let Some(consent) = &expected.auto_merge {
        let current: Option<String> = sqlx::query_scalar("SELECT a.generation FROM pull_request_auto_merges a JOIN pull_requests p ON p.id=a.pull_request_id WHERE p.repo_id=?1 AND a.pull_request_id=?2")
            .bind(repo).bind(consent.pull_request_id).fetch_optional(pool).await?;
        ensure!(current.as_deref() == Some(consent.generation.as_str()), Changed);
    }
    Ok(())
}
