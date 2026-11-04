use anyhow::Result;
use sqlx::PgConnection;
pub async fn sync_execution(connection: &mut PgConnection, run: i64, status: &str) -> Result<()> {
    let state = crate::queries::deployment_reviews::execution_state(status)?;
    sqlx::query("INSERT INTO deployment_statuses(deployment_id,creator_id,state,environment) SELECT DISTINCT d.id,d.creator_id,$2,d.environment FROM actions_deployment_reviews v JOIN repository_deployments d ON d.id=v.deployment_id WHERE v.run_id=$1 AND v.state='approved'")
        .bind(run).bind(state).execute(connection).await?;
    Ok(())
}
