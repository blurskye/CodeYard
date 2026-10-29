use crate::backend::db::{backups::PullRequestHead, models::Repository};
use anyhow::Result;
use sqlx::PgPool;

pub async fn list_repositories(pool: &PgPool) -> Result<Vec<Repository>> {
    Ok(sqlx::query_as("SELECT * FROM repositories ORDER BY id")
        .fetch_all(pool)
        .await?)
}

pub async fn set_disk_path(pool: &PgPool, repo_id: i64, disk_path: &str) -> Result<()> {
    sqlx::query("UPDATE repositories SET disk_path = $2 WHERE id = $1")
        .bind(repo_id)
        .bind(disk_path)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_pull_request_heads(pool: &PgPool, repo_id: i64) -> Result<Vec<PullRequestHead>> {
    Ok(sqlx::query_as(
        "SELECT id, number::BIGINT AS number, state, head_sha, base_sha, fork_sha, merge_commit_sha FROM pull_requests WHERE repo_id = $1 ORDER BY number",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}

pub async fn list_ci_revisions(pool: &PgPool, repo_id: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT head_sha FROM ci_runs WHERE repo_id = $1 AND head_sha IS NOT NULL AND status IN ('queued', 'pending', 'running', 'in_progress') ORDER BY head_sha",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}

pub async fn is_empty(pool: &PgPool) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname = current_schema())",
    )
    .fetch_one(pool)
    .await?)
}
