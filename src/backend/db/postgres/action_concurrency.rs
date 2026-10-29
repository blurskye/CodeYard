use crate::backend::db::{
    action_concurrency::{Configuration, Group, Member},
    models::CiRun,
};
use anyhow::Result;
use sqlx::{PgConnection, PgPool};

pub(crate) async fn lock(connection: &mut PgConnection, run: &CiRun) -> Result<()> {
    sqlx::query("SELECT g.repo_id FROM actions_concurrency_groups g JOIN actions_run_concurrency r ON r.repo_id=g.repo_id AND r.name_key=g.name_key WHERE r.run_id=$1 FOR UPDATE OF g").bind(run.id).fetch_optional(connection).await?;
    Ok(())
}

pub(crate) async fn record(
    connection: &mut PgConnection,
    run: &CiRun,
    config: &Configuration,
) -> Result<Vec<CiRun>> {
    config.validate()?;
    let key = config.group.to_lowercase();
    sqlx::query("INSERT INTO actions_concurrency_groups(repo_id,name_key,name) VALUES($1,$2,$3) ON CONFLICT(repo_id,name_key) DO NOTHING").bind(run.repo_id).bind(&key).bind(&config.group).execute(&mut *connection).await?;
    sqlx::query("SELECT repo_id FROM actions_concurrency_groups WHERE repo_id=$1 AND name_key=$2 FOR UPDATE").bind(run.repo_id).bind(&key).fetch_one(&mut *connection).await?;
    let victims = sqlx::query_as("SELECT c.* FROM ci_runs c JOIN actions_run_concurrency g ON g.run_id=c.id WHERE g.repo_id=$1 AND g.name_key=$2 AND c.status IN('queued','running') AND (NOT g.holding OR $3) ORDER BY c.id FOR UPDATE OF c").bind(run.repo_id).bind(&key).bind(config.cancel_in_progress).fetch_all(&mut *connection).await?;
    sqlx::query("INSERT INTO actions_run_concurrency(run_id,repo_id,name_key,cancel_in_progress) VALUES($1,$2,$3,$4)").bind(run.id).bind(run.repo_id).bind(key).bind(config.cancel_in_progress).execute(&mut *connection).await?;
    Ok(victims)
}

pub(crate) async fn eligible(connection: &mut PgConnection, run: &CiRun) -> Result<bool> {
    let key: Option<String> =
        sqlx::query_scalar("SELECT name_key FROM actions_run_concurrency WHERE run_id=$1")
            .bind(run.id)
            .fetch_optional(&mut *connection)
            .await?;
    let Some(key) = key else { return Ok(true) };
    let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM actions_run_concurrency g JOIN ci_runs c ON c.id=g.run_id WHERE g.repo_id=$1 AND g.name_key=$2 AND c.status IN('queued','running') AND c.id<>$3 AND (g.holding OR c.id<$3))").bind(run.repo_id).bind(&key).bind(run.id).fetch_one(&mut *connection).await?;
    if blocked {
        return Ok(false);
    }
    let acquired = sqlx::query(
        "UPDATE actions_run_concurrency SET holding=TRUE WHERE run_id=$1 AND NOT holding",
    )
    .bind(run.id)
    .execute(&mut *connection)
    .await?
    .rows_affected()
        > 0;
    if acquired {
        sqlx::query("UPDATE actions_concurrency_groups SET last_acquired_at=$3 WHERE repo_id=$1 AND name_key=$2").bind(run.repo_id).bind(key).bind(chrono::Utc::now()).execute(connection).await?;
    }
    Ok(true)
}

/// Records a job's request for its group. The request replaces the group's waiting job and,
/// with cancel-in-progress, also cancels its running one.
pub(crate) async fn request_job(
    connection: &mut PgConnection,
    run: &CiRun,
    job: i64,
    config: &Configuration,
) -> Result<()> {
    config.validate()?;
    let key = config.group.to_lowercase();
    sqlx::query("INSERT INTO actions_concurrency_groups(repo_id,name_key,name) VALUES($1,$2,$3) ON CONFLICT(repo_id,name_key) DO NOTHING").bind(run.repo_id).bind(&key).bind(&config.group).execute(&mut *connection).await?;
    sqlx::query("SELECT repo_id FROM actions_concurrency_groups WHERE repo_id=$1 AND name_key=$2 FOR UPDATE").bind(run.repo_id).bind(&key).fetch_one(&mut *connection).await?;
    // A replayed request was already recorded. It must not cancel jobs that queued after it.
    let recorded: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM actions_job_concurrency WHERE job_id=$1)")
            .bind(job)
            .fetch_one(&mut *connection)
            .await?;
    if recorded {
        return Ok(());
    }
    sqlx::query("UPDATE actions_job_concurrency SET cancelled=TRUE WHERE repo_id=$1 AND name_key=$2 AND job_id<>$3 AND NOT cancelled AND (NOT holding OR $4) AND job_id IN(SELECT j.id FROM actions_jobs j JOIN ci_runs c ON c.id=j.run_id WHERE j.status<>'completed' AND c.status IN('queued','running'))").bind(run.repo_id).bind(&key).bind(job).bind(config.cancel_in_progress).execute(&mut *connection).await?;
    sqlx::query("INSERT INTO actions_job_concurrency(job_id,repo_id,name_key) VALUES($1,$2,$3) ON CONFLICT(job_id) DO NOTHING").bind(job).bind(run.repo_id).bind(key).execute(connection).await?;
    Ok(())
}

/// A job takes its group once no other unfinished job holds it. A cancelled holder keeps the
/// group until it has stopped.
pub(crate) async fn admit_job(
    connection: &mut PgConnection,
    run: &CiRun,
    job: i64,
) -> Result<Option<bool>> {
    let (key, holding, cancelled): (String, bool, bool) = sqlx::query_as("SELECT name_key,holding,cancelled FROM actions_job_concurrency WHERE job_id=$1 AND repo_id=$2").bind(job).bind(run.repo_id).fetch_one(&mut *connection).await?;
    if cancelled {
        return Ok(Some(false));
    }
    if holding {
        return Ok(Some(true));
    }
    sqlx::query("SELECT repo_id FROM actions_concurrency_groups WHERE repo_id=$1 AND name_key=$2 FOR UPDATE").bind(run.repo_id).bind(&key).fetch_one(&mut *connection).await?;
    let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM actions_job_concurrency g JOIN actions_jobs j ON j.id=g.job_id JOIN ci_runs c ON c.id=j.run_id WHERE g.repo_id=$1 AND g.name_key=$2 AND g.job_id<>$3 AND j.status<>'completed' AND c.status IN('queued','running') AND (g.holding OR (NOT g.cancelled AND g.job_id<$3)))").bind(run.repo_id).bind(&key).bind(job).fetch_one(&mut *connection).await?;
    if blocked {
        return Ok(None);
    }
    sqlx::query("UPDATE actions_job_concurrency SET holding=TRUE WHERE job_id=$1")
        .bind(job)
        .execute(&mut *connection)
        .await?;
    sqlx::query("UPDATE actions_concurrency_groups SET last_acquired_at=$3 WHERE repo_id=$1 AND name_key=$2").bind(run.repo_id).bind(key).bind(chrono::Utc::now()).execute(connection).await?;
    Ok(Some(true))
}

pub async fn groups(pool: &PgPool, repo: i64, run: Option<i64>) -> Result<Vec<Group>> {
    Ok(sqlx::query_as("SELECT g.name,g.last_acquired_at FROM actions_concurrency_groups g WHERE g.repo_id=$1 AND (EXISTS(SELECT 1 FROM actions_job_concurrency l JOIN actions_jobs j ON j.id=l.job_id JOIN ci_runs c ON c.id=j.run_id WHERE l.repo_id=g.repo_id AND l.name_key=g.name_key AND (($2::bigint IS NULL AND c.status IN('queued','running') AND j.status<>'completed' AND (l.holding OR NOT l.cancelled)) OR COALESCE(c.workflow_run_id,c.id)=$2)) OR EXISTS(SELECT 1 FROM actions_run_concurrency r JOIN ci_runs c ON c.id=r.run_id WHERE r.repo_id=g.repo_id AND r.name_key=g.name_key AND (($2::bigint IS NULL AND c.status IN('queued','running')) OR COALESCE(c.workflow_run_id,c.id)=$2))) ORDER BY g.name_key").bind(repo).bind(run).fetch_all(pool).await?)
}
pub async fn members(pool: &PgPool, repo: i64, group: &str) -> Result<Vec<Member>> {
    Ok(sqlx::query_as("SELECT * FROM (SELECT c.*,g.holding,CAST(NULL AS BIGINT) AS job_id,CAST(NULL AS TEXT) AS job_name FROM ci_runs c JOIN actions_run_concurrency g ON g.run_id=c.id WHERE g.repo_id=$1 AND g.name_key=$2 AND c.status IN('queued','running') UNION ALL SELECT c.*,g.holding,j.id AS job_id,j.name AS job_name FROM ci_runs c JOIN actions_jobs j ON j.run_id=c.id JOIN actions_job_concurrency g ON g.job_id=j.id WHERE g.repo_id=$1 AND g.name_key=$2 AND c.status IN('queued','running') AND j.status<>'completed' AND (g.holding OR NOT g.cancelled)) m ORDER BY holding DESC,id,COALESCE(job_id,0)").bind(repo).bind(group.to_lowercase()).fetch_all(pool).await?)
}
