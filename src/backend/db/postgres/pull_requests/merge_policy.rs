use crate::backend::db::pull_requests::merge_policy::{Changed, Lease, Snapshot};
use anyhow::{ensure, Result};
use sqlx::PgPool;

pub async fn revision(pool: &PgPool, repo: i64) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COALESCE((SELECT version FROM repository_merge_epochs WHERE repo_id=$1),0)")
        .bind(repo).fetch_one(pool).await?)
}

pub async fn retain(pool: &PgPool, repo: i64, expected: &Snapshot) -> Result<Lease> {
    let mut tx = pool.begin().await?;
    super::super::users::transitions::lock_membership_shared(&mut tx).await?;
    let repository: Option<i64> = sqlx::query_scalar("SELECT id FROM repositories WHERE id=$1 FOR SHARE")
        .bind(repo).fetch_optional(&mut *tx).await?;
    ensure!(repository.is_some(), Changed);
    sqlx::query("INSERT INTO repository_merge_epochs(repo_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(repo).execute(&mut *tx).await?;
    let current: i64 = sqlx::query_scalar("SELECT version FROM repository_merge_epochs WHERE repo_id=$1 FOR SHARE")
        .bind(repo).fetch_one(&mut *tx).await?;
    ensure!(current == expected.revision, Changed);
    if let Some(consent) = &expected.auto_merge {
        let current: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT a.enabled_at FROM pull_request_auto_merges a JOIN pull_requests p ON p.id=a.pull_request_id WHERE p.repo_id=$1 AND a.pull_request_id=$2 FOR SHARE OF a")
            .bind(repo).bind(consent.pull_request_id).fetch_optional(&mut *tx).await?;
        ensure!(current == Some(consent.enabled_at), Changed);
    }
    Ok(Lease::Postgres(tx))
}
