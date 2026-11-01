use crate::backend::db::activity::{self, ActivityBatch, ActivityViewer, ScannedActivity};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn record_repository_push(
    pool: &SqlitePool,
    repo_id: i64,
    actor_id: i64,
    branch: &str,
    commit_count: i32,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO repository_pushes (repo_id, actor_id, branch, commit_count) VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(repo_id)
    .bind(actor_id)
    .bind(branch)
    .bind(commit_count)
    .execute(pool)
    .await?;
    Ok(())
}

/// One bounded scan of activity, authorized in the same statement as its content. Each
/// source reads only its newest `limit + offset` rows through its actor-and-time index, so
/// the scan does not sort the account's whole history.
pub async fn list_user_activity(
    pool: &SqlitePool,
    user_id: i64,
    viewer: Option<ActivityViewer<'_>>,
    limit: i64,
    offset: i64,
) -> Result<ActivityBatch> {
    let (limit, offset, prefix) = activity::scan_window(limit, offset);
    let readable = super::repositories::access::readable_condition();
    let rows = sqlx::query_as::<_, ScannedActivity>(&activity::activity_sql(
        &readable,
        "julianday(s.expires_at)>julianday('now')",
    ))
    .bind(user_id)
    .bind(limit)
    .bind(offset)
    .bind(viewer.map(|viewer| viewer.user))
    .bind(prefix)
    .bind(viewer.map(|viewer| viewer.session))
    .fetch_all(pool)
    .await?;
    Ok(ActivityBatch::from_scan(rows))
}
