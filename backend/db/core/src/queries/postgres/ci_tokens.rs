use crate::queries::ci_tokens::JobToken;
use anyhow::Result;
use sqlx::PgPool;
pub async fn get(pool: &PgPool, token_id: i64) -> Result<Option<JobToken>> {
    let mut connection = pool.acquire().await?;
    get_in_connection(&mut connection, token_id).await
}

pub async fn get_in_connection(
    connection: &mut sqlx::PgConnection,
    token_id: i64,
) -> Result<Option<JobToken>> {
    Ok(sqlx::query_as("SELECT r.id AS run_id,r.repo_id,t.permissions FROM actions_github_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.token_id=$1 AND NOT r.cancel_requested AND (t.revoked_at IS NULL OR t.revoked_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint-60) AND (r.status='running' OR r.completed_at>clock_timestamp()-INTERVAL '1 minute')")
        .bind(token_id).fetch_optional(connection).await?)
}
