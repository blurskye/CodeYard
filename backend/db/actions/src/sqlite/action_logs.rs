use crate::action_logs::Grant;
use crate::action_logs::Job;
use crate::action_logs::Manifest;
use crate::action_logs::ObservedStep;
use crate::action_logs::Target;
use anyhow::ensure;
use anyhow::Result;
use codeyard_database::pool::SqlitePool;
use sqlx::types::Json;
use sqlx::SqliteConnection;

pub async fn issue(
    pool: &SqlitePool,
    actor: impl Into<codeyard_database::queries::credentials::Actor>,
    repo: i64,
    resource: &str,
    target: Target,
    hash: &str,
) -> Result<bool> {
    let actor = actor.into();
    ensure!(
        resource.len() <= 4096
            && hash.len() == 64
            && target
                .step
                .is_none_or(|step| target.job.is_some() && step > 0),
        "invalid log capability"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let readable:bool=sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=$1 WHERE r.id=$2 AND NOT u.is_banned AND NOT u.must_change_password AND {})",codeyard_database::queries::sqlite::repositories::access::readable_condition())).bind(actor.user_id).bind(repo).fetch_one(&mut *tx).await?;
    if !readable {
        return Ok(false);
    }
    if !codeyard_database::queries::sqlite::credentials::repository(
        &mut tx, &actor, repo, "actions", false,
    )
    .await?
    {
        return Ok(false);
    }
    let version: Option<i64> =
        sqlx::query_scalar("SELECT log_version FROM ci_runs WHERE id=$1 AND repo_id=$2 AND (log_expires_at IS NULL OR log_expires_at>CAST(strftime('%s','now') AS INTEGER))")
            .bind(target.run)
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(version) = version else {
        return Ok(false);
    };
    if version != 0 {
        return Ok(false);
    }
    let jobs:Vec<Job>=sqlx::query_as("SELECT id,name FROM actions_jobs WHERE run_id=$1 AND started_at IS NOT NULL AND ($2 IS NULL OR id=$2) ORDER BY id").bind(target.run).bind(target.job).fetch_all(&mut *tx).await?;
    if target.job.is_some() && jobs.len() != 1 {
        return Ok(false);
    }
    let steps = sqlx::query_as::<_, ObservedStep>("SELECT s.job_id,s.number,s.name,s.started_at,s.completed_at FROM actions_steps s JOIN actions_jobs j ON j.id=s.job_id WHERE j.run_id=?1 AND j.started_at IS NOT NULL AND (?2 IS NULL OR j.id=?2) ORDER BY s.job_id,s.number")
        .bind(target.run).bind(target.job).fetch_all(&mut *tx).await?;
    let Some(manifest) = Manifest::capture(jobs, steps, target.step) else {
        return Ok(false);
    };
    let max: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(first_line+line_count-1),0) FROM ci_log_chunks WHERE run_id=$1",
    )
    .bind(target.run)
    .fetch_one(&mut *tx)
    .await?;
    if target.job.is_none() && max == 0 {
        return Ok(false);
    }
    ensure!(
        serde_json::to_vec(&manifest)?.len() <= 16 * 1024 * 1024,
        "log manifest exceeds native limits"
    );
    sqlx::query("DELETE FROM actions_log_downloads WHERE token_hash IN(SELECT token_hash FROM actions_log_downloads WHERE expires_at<=CAST(strftime('%s','now') AS INTEGER) ORDER BY expires_at LIMIT 1000)").execute(&mut *tx).await?;
    sqlx::query("INSERT INTO actions_log_downloads(token_hash,resource,repo_id,run_id,job_id,step_number,log_version,max_log_id,manifest,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,CAST(strftime('%s','now') AS INTEGER)+60)").bind(hash).bind(resource).bind(repo).bind(target.run).bind(target.job).bind(target.step).bind(version).bind(max).bind(Json(manifest)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

pub(crate) async fn grant(pool: &SqlitePool, hash: &str, resource: &str) -> Result<Option<Grant>> {
    let mut tx = pool.begin().await?;
    let grant:Option<Grant>=sqlx::query_as("SELECT g.* FROM actions_log_downloads g JOIN ci_runs c ON c.id=g.run_id AND c.repo_id=g.repo_id AND c.log_version=g.log_version AND (c.log_expires_at IS NULL OR c.log_expires_at>CAST(strftime('%s','now') AS INTEGER)) WHERE g.token_hash=$1 AND g.resource=$2 AND g.expires_at>CAST(strftime('%s','now') AS INTEGER)").bind(hash).bind(resource).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(grant)
}

/// Manual deletion and retention invalidate the same frozen download manifests.
pub async fn clear(connection: &mut SqliteConnection, runs: &[i64]) -> Result<()> {
    let runs = serde_json::to_string(runs)?;
    sqlx::query("DELETE FROM ci_log_chunks WHERE run_id IN (SELECT value FROM json_each(?1))")
        .bind(&runs)
        .execute(&mut *connection)
        .await?;
    sqlx::query("UPDATE ci_runs SET log_lines=0,log_bytes=0,log_version=log_version+1,log_expires_at=NULL WHERE id IN (SELECT value FROM json_each(?1))")
        .bind(&runs).execute(connection).await?;
    Ok(())
}

pub(super) async fn expire(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let runs: Vec<i64> = sqlx::query_scalar("SELECT id FROM ci_runs WHERE log_expires_at IS NOT NULL AND log_lines>0 AND log_expires_at<=?1 ORDER BY log_expires_at,id LIMIT ?2")
        .bind(chrono::Utc::now().timestamp()).bind(crate::action_logs::RETENTION_BATCH).fetch_all(&mut *tx).await?;
    if !runs.is_empty() {
        clear(&mut tx, &runs).await?;
    }
    tx.commit().await?;
    Ok(())
}
