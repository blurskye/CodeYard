use crate::backend::db::action_logs::{Grant, Job, Line, Manifest, Snapshot, Step, Target};
use anyhow::{ensure, Result};
use sqlx::{types::Json, SqlitePool};

pub async fn issue(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    resource: &str,
    target: Target,
    hash: &str,
) -> Result<bool> {
    ensure!(
        resource.len() <= 4096
            && hash.len() == 64
            && target
                .step
                .is_none_or(|step| target.job.is_some() && step > 0),
        "invalid log capability"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let readable:bool=sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=$1 WHERE r.id=$2 AND NOT u.is_banned AND NOT u.must_change_password AND {})",super::repositories::access::readable_condition())).bind(actor).bind(repo).fetch_one(&mut *tx).await?;
    if !readable {
        return Ok(false);
    }
    let version: Option<i64> =
        sqlx::query_scalar("SELECT log_version FROM ci_runs WHERE id=$1 AND repo_id=$2")
            .bind(target.run)
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(version) = version else {
        return Ok(false);
    };
    if version > 0 {
        return Ok(false);
    }
    let mut jobs:Vec<Job>=sqlx::query_as("SELECT id,name FROM actions_jobs WHERE run_id=$1 AND started_at IS NOT NULL AND ($2 IS NULL OR id=$2) ORDER BY id").bind(target.run).bind(target.job).fetch_all(&mut *tx).await?;
    if target.job.is_some() && jobs.len() != 1 {
        return Ok(false);
    }
    for job in &mut jobs {
        job.steps=sqlx::query_as::<_,Step>("SELECT number,name,started_at,completed_at FROM actions_steps WHERE job_id=$1 ORDER BY number").bind(job.id).fetch_all(&mut *tx).await?;
        if target
            .step
            .is_some_and(|number| !job.steps.iter().any(|step| step.number == number))
        {
            return Ok(false);
        }
    }
    let max:i64=sqlx::query_scalar("SELECT COALESCE(MAX(id),0) FROM ci_logs WHERE run_id=$1 AND ($2 IS NULL OR job_id=$2) AND ($3 IS NULL OR step_number=$3)").bind(target.run).bind(target.job).bind(target.step).fetch_one(&mut *tx).await?;
    if target.job.is_none() && max == 0 {
        return Ok(false);
    }
    let manifest = Manifest { jobs };
    ensure!(
        serde_json::to_vec(&manifest)?.len() <= 16 * 1024 * 1024,
        "log manifest exceeds native limits"
    );
    sqlx::query("DELETE FROM actions_log_downloads WHERE token_hash IN(SELECT token_hash FROM actions_log_downloads WHERE expires_at<=CAST(strftime('%s','now') AS INTEGER) ORDER BY expires_at LIMIT 1000)").execute(&mut *tx).await?;
    sqlx::query("INSERT INTO actions_log_downloads(token_hash,resource,repo_id,run_id,job_id,step_number,log_version,max_log_id,manifest,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,CAST(strftime('%s','now') AS INTEGER)+60)").bind(hash).bind(resource).bind(repo).bind(target.run).bind(target.job).bind(target.step).bind(version).bind(max).bind(Json(manifest)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn read(pool: &SqlitePool, hash: &str, resource: &str) -> Result<Option<Snapshot>> {
    let mut tx = pool.begin().await?;
    let grant:Option<Grant>=sqlx::query_as("SELECT g.* FROM actions_log_downloads g JOIN ci_runs c ON c.id=g.run_id AND c.repo_id=g.repo_id AND c.log_version=g.log_version WHERE g.token_hash=$1 AND g.resource=$2 AND g.expires_at>CAST(strftime('%s','now') AS INTEGER)").bind(hash).bind(resource).fetch_optional(&mut *tx).await?;
    let Some(grant) = grant else {
        return Ok(None);
    };
    let lines:Vec<Line>=sqlx::query_as("SELECT id,job_id,step_number,stage,line,created_at FROM ci_logs WHERE run_id=$1 AND id<=$2 AND ($3 IS NULL OR job_id=$3) AND ($4 IS NULL OR step_number=$4) ORDER BY id").bind(grant.run_id).bind(grant.max_log_id).bind(grant.job_id).bind(grant.step_number).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(Snapshot { grant, lines }))
}
