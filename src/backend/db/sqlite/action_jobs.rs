use crate::backend::db::{
    action_concurrency::Configuration,
    action_jobs::{Job, Page, Step},
    models::CiRun,
};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use codeyard_actions::progress::{Annotation, Event, State, Update};
use sqlx::{types::Json, SqliteConnection, SqlitePool};

async fn lease(
    connection: &mut SqliteConnection,
    worker: &str,
    run: i64,
    generation: &str,
    workflow: &str,
) -> Result<CiRun> {
    let run:CiRun=sqlx::query_as("SELECT c.* FROM ci_runs c JOIN actions_runtime_tokens t ON t.run_id=c.id WHERE c.id=$1 AND c.worker=$2 AND c.status='running' AND t.token_hash=$3").bind(run).bind(worker).bind(generation).fetch_one(connection).await?;
    ensure!(
        run.workflow_path.split(',').any(|path| path == workflow),
        "workflow is outside the claim"
    );
    Ok(run)
}
fn name(value: &str) -> Result<()> {
    ensure!(
        value.len() <= 1024 && !value.contains('\0'),
        "invalid runner label"
    );
    Ok(())
}
fn milliseconds(value: u64) -> Result<i64> {
    Ok(value.try_into()?)
}

pub async fn apply(
    pool: &SqlitePool,
    worker: &str,
    run: i64,
    generation: &str,
    workflow: &str,
    event: &Event,
) -> Result<i64> {
    ensure!(
        !event.job.is_empty()
            && event.job.len() <= 128
            && !event.job.contains('\0')
            && event.variant < 256,
        "invalid job identity"
    );
    let at = DateTime::<Utc>::from_timestamp_millis(milliseconds(event.at_ms)?)
        .ok_or_else(|| anyhow::anyhow!("invalid runner clock"))?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let leased = lease(&mut tx, worker, run, generation, workflow).await?;
    if let Update::Queued {
        name: display,
        labels,
        workflow_name,
        matrix,
    } = &event.update
    {
        name(display)?;
        name(workflow_name)?;
        ensure!(labels.len() <= 64, "too many runner labels");
        for label in labels {
            name(label)?;
        }
        ensure!(
            matrix
                .as_ref()
                .is_none_or(|matrix| serde_json::to_vec(&matrix.values)
                    .is_ok_and(|bytes| bytes.len() <= 8192)),
            "matrix metadata exceeds the native limit"
        );
        sqlx::query("INSERT INTO actions_jobs(run_id,generation,workflow_path,logical_job,variant,name,labels,workflow_name,status,created_at,matrix) VALUES($1,$2,$3,$4,$5,$6,$7,$8,'queued',$9,$10) ON CONFLICT(run_id,generation,workflow_path,logical_job,variant) DO NOTHING").bind(run).bind(generation).bind(workflow).bind(&event.job).bind(event.variant as i32).bind(display).bind(Json(labels)).bind(workflow_name).bind(at).bind(matrix.as_ref().map(|matrix|Json(&matrix.values))).execute(&mut *tx).await?;
    }
    let id:i64=sqlx::query_scalar("SELECT id FROM actions_jobs WHERE run_id=$1 AND generation=$2 AND workflow_path=$3 AND logical_job=$4 AND variant=$5").bind(run).bind(generation).bind(workflow).bind(&event.job).bind(event.variant as i32).fetch_one(&mut *tx).await?;
    match &event.update {
        Update::Queued { .. } => {}
        Update::Started => {
            sqlx::query("UPDATE actions_jobs SET status='in_progress',started_at=$2 WHERE id=$1 AND status='queued'").bind(id).bind(at).execute(&mut *tx).await?;
        }
        Update::StepStarted {
            number,
            name: display,
        } => {
            ensure!((1..=10000).contains(number), "invalid step number");
            name(display)?;
            let active: bool =
                sqlx::query_scalar("SELECT status='in_progress' FROM actions_jobs WHERE id=$1")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
            ensure!(active, "step outside an active job");
            sqlx::query("INSERT INTO actions_steps(job_id,number,name,status,started_at) VALUES($1,$2,$3,'in_progress',$4) ON CONFLICT(job_id,number) DO NOTHING").bind(id).bind(*number as i32).bind(display).bind(at).execute(&mut *tx).await?;
        }
        Update::StepFinished {
            number,
            conclusion,
            duration_ms,
        } => {
            sqlx::query("UPDATE actions_steps SET status='completed',conclusion=$3,completed_at=$4,duration_ms=$5 WHERE job_id=$1 AND number=$2 AND status='in_progress'").bind(id).bind(*number as i32).bind(conclusion.as_str()).bind(at).bind(milliseconds(*duration_ms)?).execute(&mut *tx).await?;
        }
        Update::Finished {
            conclusion,
            duration_ms,
        } => {
            sqlx::query("UPDATE actions_steps SET status='completed',completed_at=$2 WHERE job_id=$1 AND status='in_progress'").bind(id).bind(at).execute(&mut *tx).await?;
            sqlx::query("UPDATE actions_jobs SET status='completed',conclusion=$2,completed_at=$3,duration_ms=$4 WHERE id=$1 AND status<>'completed'").bind(id).bind(conclusion.as_str()).bind(at).bind(milliseconds(*duration_ms)?).execute(&mut *tx).await?;
        }
        Update::Annotation { annotation } => {
            let annotation = crate::backend::db::action_jobs::prepare_annotation(annotation)?;
            sqlx::query("INSERT INTO actions_job_annotations(job_id,annotation) SELECT $1,$2 WHERE (SELECT COUNT(*) FROM actions_job_annotations WHERE job_id=$1)<1000 AND EXISTS(SELECT 1 FROM actions_jobs WHERE id=$1 AND status='in_progress') AND NOT EXISTS(SELECT 1 FROM actions_job_annotations WHERE job_id=$1 AND annotation=$2)")
                .bind(id).bind(Json(annotation)).execute(&mut *tx).await?;
        }
        Update::Concurrency {
            group,
            cancel_in_progress,
        } => {
            let queued: bool =
                sqlx::query_scalar("SELECT status='queued' FROM actions_jobs WHERE id=$1")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
            ensure!(queued, "concurrency request outside a queued job");
            let config = Configuration {
                group: group.clone(),
                cancel_in_progress: *cancel_in_progress,
            };
            super::action_concurrency::request_job(&mut tx, &leased, id, &config).await?;
        }
        Update::Log { .. } => anyhow::bail!("runner output uses bounded native log storage"),
    }
    tx.commit().await?;
    Ok(id)
}

/// Answers a job's concurrency request: `Some(true)` once it holds its group,
/// `Some(false)` once a newer job cancelled it, `None` while it waits.
pub async fn admit(
    pool: &SqlitePool,
    worker: &str,
    run: i64,
    generation: &str,
    workflow: &str,
    job: i64,
) -> Result<Option<bool>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let leased = lease(&mut tx, worker, run, generation, workflow).await?;
    let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM actions_jobs WHERE id=$1 AND run_id=$2 AND generation=$3 AND workflow_path=$4)").bind(job).bind(run).bind(generation).bind(workflow).fetch_one(&mut *tx).await?;
    ensure!(owned, "job is outside the claim");
    let answer = super::action_concurrency::admit_job(&mut tx, &leased, job).await?;
    tx.commit().await?;
    Ok(answer)
}

pub async fn save_state(
    pool: &SqlitePool,
    worker: &str,
    run: i64,
    generation: &str,
    workflow: &str,
    state: &State,
) -> Result<()> {
    ensure!(
        state.has_valid_variant_order(),
        "invalid observed variant order"
    );
    ensure!(
        serde_json::to_vec(state)?.len() <= 8 * 1024 * 1024,
        "job state exceeds the native limit"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    lease(&mut tx, worker, run, generation, workflow).await?;
    sqlx::query("UPDATE actions_steps SET status='completed',completed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE status='in_progress' AND job_id IN(SELECT id FROM actions_jobs WHERE run_id=$1 AND generation<>$2)").bind(run).bind(generation).execute(&mut *tx).await?;
    sqlx::query("UPDATE actions_jobs SET status='completed',completed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE run_id=$1 AND generation<>$2 AND status<>'completed'").bind(run).bind(generation).execute(&mut *tx).await?;
    let known:Vec<(String,i32,String)>=sqlx::query_as("SELECT logical_job,variant,status FROM actions_jobs WHERE run_id=$1 AND generation=$2 AND workflow_path=$3").bind(run).bind(generation).bind(workflow).fetch_all(&mut *tx).await?;
    crate::backend::db::action_jobs::validate_observed_state(state, &known)?;
    sqlx::query("INSERT INTO actions_job_states(run_id,generation,workflow_path,state) VALUES($1,$2,$3,$4) ON CONFLICT(run_id,generation,workflow_path) DO UPDATE SET state=excluded.state").bind(run).bind(generation).bind(workflow).bind(Json(state)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn close_active(connection: &mut SqliteConnection, run: i64) -> Result<()> {
    sqlx::query("UPDATE actions_steps SET status='completed',completed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE status='in_progress' AND job_id IN(SELECT id FROM actions_jobs WHERE run_id=$1)").bind(run).execute(&mut *connection).await?;
    sqlx::query("UPDATE actions_jobs SET status='completed',completed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now'),duration_ms=CASE WHEN started_at IS NOT NULL THEN MAX(0,CAST(ROUND((julianday('now')-julianday(started_at))*86400000) AS INTEGER)) END WHERE run_id=$1 AND status<>'completed'").bind(run).execute(connection).await?;
    Ok(())
}

pub async fn get(pool: &SqlitePool, repo: i64, id: i64, queued: bool) -> Result<Option<Job>> {
    Ok(sqlx::query_as("SELECT j.* FROM actions_jobs j JOIN ci_runs c ON c.id=j.run_id WHERE c.repo_id=$1 AND j.id=$2 AND ($3 OR j.started_at IS NOT NULL OR j.status='completed')").bind(repo).bind(id).bind(queued).fetch_optional(pool).await?)
}
pub async fn list(
    pool: &SqlitePool,
    repo: i64,
    run: i64,
    all: bool,
    offset: i64,
    limit: i64,
) -> Result<Page> {
    ensure!(
        offset >= 0 && (1..=100).contains(&limit),
        "invalid job page"
    );
    let mut tx = pool.begin().await?;
    let filter="c.repo_id=$1 AND (($3 AND COALESCE(c.workflow_run_id,c.id)=$2) OR (NOT $3 AND c.id=$2)) AND (j.started_at IS NOT NULL OR j.status='completed')";
    let total = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM actions_jobs j JOIN ci_runs c ON c.id=j.run_id WHERE {filter}"
    ))
    .bind(repo)
    .bind(run)
    .bind(all)
    .fetch_one(&mut *tx)
    .await?;
    let jobs=sqlx::query_as(&format!("SELECT j.* FROM actions_jobs j JOIN ci_runs c ON c.id=j.run_id WHERE {filter} ORDER BY j.id LIMIT $4 OFFSET $5")).bind(repo).bind(run).bind(all).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Page { total, jobs })
}
pub async fn steps(pool: &SqlitePool, repo: i64, job: i64) -> Result<Vec<Step>> {
    Ok(sqlx::query_as("SELECT s.* FROM actions_steps s JOIN actions_jobs j ON j.id=s.job_id JOIN ci_runs c ON c.id=j.run_id WHERE c.repo_id=$1 AND j.id=$2 ORDER BY s.number").bind(repo).bind(job).fetch_all(pool).await?)
}
pub async fn annotations(pool: &SqlitePool, repo: i64, job: i64) -> Result<Vec<Json<Annotation>>> {
    Ok(sqlx::query_scalar("SELECT a.annotation FROM actions_job_annotations a JOIN actions_jobs j ON j.id=a.job_id JOIN ci_runs c ON c.id=j.run_id WHERE c.repo_id=$1 AND j.id=$2 ORDER BY a.id")
        .bind(repo).bind(job).fetch_all(pool).await?)
}
pub async fn usage(
    pool: &SqlitePool,
    repo: i64,
    run: Option<i64>,
    workflow: Option<&str>,
    since: DateTime<Utc>,
) -> Result<Vec<crate::backend::db::action_jobs::Usage>> {
    Ok(sqlx::query_as("SELECT j.id,j.run_id,j.labels,j.duration_ms,c.started_at AS run_started_at,c.completed_at AS run_completed_at FROM actions_jobs j JOIN ci_runs c ON c.id=j.run_id WHERE c.repo_id=$1 AND ($2 IS NULL OR COALESCE(c.workflow_run_id,c.id)=$2) AND ($3 IS NULL OR j.workflow_path=$3) AND j.started_at >= $4 AND j.duration_ms IS NOT NULL AND j.started_at IS NOT NULL ORDER BY j.id")
        .bind(repo).bind(run).bind(workflow).bind(since).fetch_all(pool).await?)
}

pub async fn observed(pool: &SqlitePool, repo: i64, run: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM actions_job_states s JOIN ci_runs c ON c.id=s.run_id WHERE c.repo_id=$1 AND c.id=$2)").bind(repo).bind(run).fetch_one(pool).await?)
}
