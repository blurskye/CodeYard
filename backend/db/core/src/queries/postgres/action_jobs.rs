use anyhow::Result;
use sqlx::PgConnection;
pub(crate) async fn close_active(connection: &mut PgConnection, run: i64) -> Result<Vec<i64>> {
    sqlx::query("UPDATE actions_steps SET status='completed',conclusion=CASE WHEN (SELECT status FROM ci_runs WHERE id=$1)='failed' THEN 'failure' ELSE conclusion END,completed_at=clock_timestamp() WHERE status='in_progress' AND job_id IN(SELECT id FROM actions_jobs WHERE run_id=$1)").bind(run).execute(&mut *connection).await?;
    Ok(sqlx::query_scalar("UPDATE actions_jobs SET status='completed',conclusion=CASE WHEN (SELECT status FROM ci_runs WHERE id=$1)='failed' THEN 'failure' ELSE conclusion END,completed_at=clock_timestamp(),duration_ms=CASE WHEN started_at IS NOT NULL THEN GREATEST(0,FLOOR(EXTRACT(EPOCH FROM (clock_timestamp()-started_at))*1000)::bigint) END WHERE run_id=$1 AND status<>'completed' RETURNING id").bind(run).fetch_all(connection).await?)
}
