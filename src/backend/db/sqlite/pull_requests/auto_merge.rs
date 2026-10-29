use crate::backend::db::pull_requests::auto_merge::AutoMerge;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn enable(pool: &SqlitePool, request: &AutoMerge) -> Result<()> {
    sqlx::query(
        "INSERT INTO pull_request_auto_merges(pull_request_id,enabled_by,merge_method,commit_title,commit_message,author_email,enabled_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(pull_request_id) DO UPDATE SET enabled_by=excluded.enabled_by,merge_method=excluded.merge_method,commit_title=excluded.commit_title,commit_message=excluded.commit_message,author_email=excluded.author_email,enabled_at=excluded.enabled_at",
    )
    .bind(request.pull_request_id)
    .bind(request.enabled_by)
    .bind(&request.merge_method)
    .bind(&request.commit_title)
    .bind(&request.commit_message)
    .bind(&request.author_email)
    .bind(request.enabled_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn disable(pool: &SqlitePool, pull_request_id: i64) -> Result<bool> {
    let done = sqlx::query("DELETE FROM pull_request_auto_merges WHERE pull_request_id=?1")
        .bind(pull_request_id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

pub async fn get(pool: &SqlitePool, pull_request_id: i64) -> Result<Option<AutoMerge>> {
    Ok(
        sqlx::query_as("SELECT * FROM pull_request_auto_merges WHERE pull_request_id=?1")
            .bind(pull_request_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn pending(pool: &SqlitePool, after: i64, limit: i64) -> Result<Vec<AutoMerge>> {
    Ok(sqlx::query_as(
        "SELECT * FROM pull_request_auto_merges WHERE pull_request_id>?1 ORDER BY pull_request_id LIMIT ?2",
    )
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}
