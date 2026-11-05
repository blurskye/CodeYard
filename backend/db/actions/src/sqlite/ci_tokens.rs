use crate::ci_tokens::NewJobToken;
use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::pool::SqlitePool;
use codeyard_db_core::queries::ci_tokens::BOT;

pub async fn issue(
    pool: &SqlitePool,
    run_id: i64,
    worker: &str,
    tokens: &[NewJobToken<'_>],
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (repo, fork): (i64, bool) = sqlx::query_as(
        "SELECT repo_id,is_fork FROM ci_runs WHERE id=$1 AND worker=$2 AND status='running' AND NOT cancel_requested",
    )
    .bind(run_id)
    .bind(worker)
    .fetch_one(&mut *tx)
    .await?;
    ensure!(
        !fork || tokens.iter().all(|token| !token.write),
        "fork token is read-only"
    );
    sqlx::query(&format!("INSERT INTO users(home_cell,username,display_name,email_verified) VALUES ({cell_home},$1,'GitHub Actions',true) ON CONFLICT(username) DO NOTHING", cell_home = codeyard_db_core::cells::LOCAL_HOME_SQL))
        .bind(BOT).execute(&mut *tx).await?;
    let bot: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username=$1 AND password_hash IS NULL AND email IS NULL AND NOT is_admin AND NOT is_banned")
        .bind(BOT).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO repo_access(user_id,repo_id,access_level) VALUES($1,$2,3) ON CONFLICT(user_id,repo_id) DO NOTHING")
        .bind(bot).bind(repo).execute(&mut *tx).await?;
    // A retried claim rotates all credentials, including jobs that finished before the retry.
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

pub async fn finish_job(pool: &SqlitePool, run_id: i64, worker: &str, job: &str) -> Result<()> {
    sqlx::query("UPDATE actions_github_tokens SET revoked_at=COALESCE(revoked_at,CAST(strftime('%s','now') AS INTEGER)) WHERE run_id=$1 AND (job=$3 OR substr(job,1,length($3)+1)=$3 || '/') AND EXISTS(SELECT 1 FROM ci_runs WHERE id=$1 AND worker=$2 AND status='running')")
        .bind(run_id).bind(worker).bind(job).execute(pool).await?;
    Ok(())
}
