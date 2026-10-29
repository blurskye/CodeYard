use crate::backend::db::oidc::Job;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn key(pool: &SqlitePool) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT private_key FROM actions_oidc_key WHERE id=1")
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn store_key(pool: &SqlitePool, pem: &str) -> Result<String> {
    sqlx::query(
        "INSERT INTO actions_oidc_key(id,private_key) VALUES(1,$1) ON CONFLICT(id) DO NOTHING",
    )
    .bind(pem)
    .execute(pool)
    .await?;
    key(pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("OIDC key missing"))
}

pub async fn bind_job(
    pool: &SqlitePool,
    hash: &str,
    workflow: &str,
    name: &str,
    environment: Option<&str>,
) -> Result<()> {
    sqlx::query("INSERT INTO actions_oidc_jobs(token_id,workflow,workflow_name,environment) SELECT id,$2,$3,$4 FROM personal_access_tokens WHERE token_hash=$1")
        .bind(hash).bind(workflow).bind(name).bind(environment).execute(pool).await?;
    Ok(())
}

pub async fn job(pool: &SqlitePool, hash: &str) -> Result<Option<Job>> {
    Ok(sqlx::query_as("SELECT r.run_id,t.job,o.workflow,o.workflow_name,o.environment FROM actions_oidc_jobs o JOIN personal_access_tokens p ON p.id=o.token_id JOIN actions_github_tokens t ON t.token_id=p.id JOIN ci_runs r ON r.id=t.run_id JOIN users u ON u.id=p.user_id WHERE p.token_hash=$1 AND t.revoked_at IS NULL AND json_extract(t.permissions,'$.id-token')=2 AND r.status='running' AND NOT r.cancel_requested AND NOT r.is_fork AND NOT u.is_banned AND NOT u.must_change_password AND (p.expires_at IS NULL OR julianday(p.expires_at)>julianday('now'))")
        .bind(hash).fetch_optional(pool).await?)
}
