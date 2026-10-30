use crate::backend::db::activity::{self, ActivityBatch, ScannedActivity};
use anyhow::Result;
use sqlx::PgPool;

pub async fn record_repository_push(
    pool: &PgPool,
    repo_id: i64,
    actor_id: i64,
    branch: &str,
    commit_count: i32,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO repository_pushes (repo_id, actor_id, branch, commit_count) VALUES ($1, $2, $3, $4)",
    )
    .bind(repo_id)
    .bind(actor_id)
    .bind(branch)
    .bind(commit_count)
    .execute(pool)
    .await?;
    Ok(())
}

/// One bounded scan of activity, authorized in the same statement as its content.
pub async fn list_user_activity(
    pool: &PgPool,
    user_id: i64,
    viewer: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<ActivityBatch> {
    let (limit, offset, prefix) = activity::scan_window(limit, offset);
    let readable = super::repositories::access::readable_condition();
    let rows = sqlx::query_as::<_, ScannedActivity>(&activity::activity_sql(&readable))
        .bind(user_id).bind(limit).bind(offset).bind(viewer).bind(prefix)
        .fetch_all(pool).await?;
    Ok(ActivityBatch::from_scan(rows))
}
