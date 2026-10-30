use crate::backend::db::ci_tokens::{JobToken, NewJobToken, BOT};
use anyhow::{ensure, Result};
use sqlx::PgPool;

pub async fn issue(
    pool: &PgPool,
    run_id: i64,
    worker: &str,
    tokens: &[NewJobToken<'_>],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    let repo: i64 = sqlx::query_scalar("SELECT repo_id FROM ci_runs WHERE id=$1")
        .bind(run_id).fetch_one(&mut *tx).await?;
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR SHARE")
        .bind(repo).fetch_one(&mut *tx).await?;
    let (repo, fork): (i64, bool) = sqlx::query_as("SELECT repo_id,is_fork FROM ci_runs WHERE id=$1 AND worker=$2 AND status='running' AND NOT cancel_requested FOR UPDATE")
        .bind(run_id).bind(worker).fetch_one(&mut *tx).await?;
    ensure!(
        !fork || tokens.iter().all(|token| !token.write),
        "fork token is read-only"
    );
    sqlx::query("INSERT INTO users(username,display_name,email_verified) VALUES ($1,'GitHub Actions',true) ON CONFLICT(username) DO NOTHING")
        .bind(BOT).execute(&mut *tx).await?;
    let bot: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username=$1 AND password_hash IS NULL AND email IS NULL AND NOT is_admin AND NOT is_banned FOR SHARE")
        .bind(BOT).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO repo_access(user_id,repo_id,access_level) VALUES($1,$2,3) ON CONFLICT(user_id,repo_id) DO NOTHING")
        .bind(bot).bind(repo).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM personal_access_tokens WHERE id IN (SELECT token_id FROM actions_github_tokens WHERE run_id=$1)")
        .bind(run_id).execute(&mut *tx).await?;
    for token in tokens {
        let id: i64 = sqlx::query_scalar("INSERT INTO personal_access_tokens(user_id,name,token_hash,scope) VALUES($1,'GITHUB_TOKEN',$2,$3) RETURNING id")
            .bind(bot).bind(token.hash).bind(if token.write {"write"} else {"read"}).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO actions_github_tokens(token_id,run_id,job,permissions) VALUES($1,$2,$3,$4)")
            .bind(id).bind(run_id).bind(token.job).bind(token.permissions).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn get(pool: &PgPool, token_id: i64) -> Result<Option<JobToken>> {
    let mut connection = pool.acquire().await?;
    get_in_connection(&mut connection, token_id).await
}

pub(crate) async fn get_in_connection(
    connection: &mut sqlx::PgConnection,
    token_id: i64,
) -> Result<Option<JobToken>> {
    Ok(sqlx::query_as("SELECT r.id AS run_id,r.repo_id,t.permissions FROM actions_github_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.token_id=$1 AND NOT r.cancel_requested AND (t.revoked_at IS NULL OR t.revoked_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint-60) AND (r.status='running' OR r.completed_at>clock_timestamp()-INTERVAL '1 minute')")
        .bind(token_id).fetch_optional(connection).await?)
}

pub async fn finish_job(pool: &PgPool, run_id: i64, worker: &str, job: &str) -> Result<()> {
    sqlx::query("UPDATE actions_github_tokens SET revoked_at=COALESCE(revoked_at,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint) WHERE run_id=$1 AND (job=$3 OR left(job,length($3)+1)=$3 || '/') AND EXISTS(SELECT 1 FROM ci_runs WHERE id=$1 AND worker=$2 AND status='running')")
        .bind(run_id).bind(worker).bind(job).execute(pool).await?;
    Ok(())
}
