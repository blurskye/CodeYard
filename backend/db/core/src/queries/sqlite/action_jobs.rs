use anyhow::Result;
use sqlx::SqliteConnection;
pub(crate) async fn close_active(connection: &mut SqliteConnection, run: i64) -> Result<Vec<i64>> {
    sqlx::query("UPDATE actions_steps SET status='completed',conclusion=CASE WHEN (SELECT status FROM ci_runs WHERE id=$1)='failed' THEN 'failure' ELSE conclusion END,completed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE status='in_progress' AND job_id IN(SELECT id FROM actions_jobs WHERE run_id=$1)").bind(run).execute(&mut *connection).await?;
    Ok(sqlx::query_scalar("UPDATE actions_jobs SET status='completed',conclusion=CASE WHEN (SELECT status FROM ci_runs WHERE id=$1)='failed' THEN 'failure' ELSE conclusion END,completed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now'),duration_ms=CASE WHEN started_at IS NOT NULL THEN MAX(0,CAST(ROUND((julianday('now')-julianday(started_at))*86400000) AS INTEGER)) END WHERE run_id=$1 AND status<>'completed' RETURNING id").bind(run).fetch_all(connection).await?)
}
