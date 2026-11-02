use crate::backend::db::deployments::{
    Deployment, Filter, NewDeployment, NewStatus, Refusal, Status,
};
use anyhow::Result;
use sqlx::PgPool;

pub async fn create(pool: &PgPool, input: NewDeployment<'_>) -> Result<Deployment> {
    input.validate()?;
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        super::discussions::lock_exclusive_write_authority(&mut tx, input.actor, input.repo)
            .await?,
        Refusal::Denied
    );
    let checks: Vec<(String, String)> = sqlx::query_as(
        "SELECT context,state FROM commit_statuses_latest WHERE repo_id=$1 AND sha=$2",
    )
    .bind(input.repo)
    .bind(input.sha)
    .fetch_all(&mut *tx)
    .await?;
    let passed = match input.required_contexts {
        Some(contexts) => contexts.iter().all(|context| {
            checks
                .iter()
                .any(|(name, state)| name == context && state == "success")
        }),
        None => checks.iter().all(|(_, state)| state == "success"),
    };
    anyhow::ensure!(passed, Refusal::Checks);
    sqlx::query("INSERT INTO deployment_environments(repo_id,name,name_key) VALUES($1,$2,$3) ON CONFLICT(repo_id,name_key) DO NOTHING").bind(input.repo).bind(input.environment).bind(input.environment.to_lowercase()).execute(&mut *tx).await?;
    let row = sqlx::query_as("INSERT INTO repository_deployments(repo_id,creator_id,sha,reference,task,payload,original_environment,environment,description,transient_environment,production_environment) VALUES($1,$2,$3,$4,$5,$6,$7,$7,$8,$9,$10) RETURNING *")
        .bind(input.repo).bind(input.actor).bind(input.sha).bind(input.reference).bind(input.task).bind(input.payload.to_string()).bind(input.environment).bind(input.description).bind(input.transient).bind(input.production).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}

pub async fn get(pool: &PgPool, repo: i64, id: i64) -> Result<Option<Deployment>> {
    Ok(
        sqlx::query_as("SELECT * FROM repository_deployments WHERE repo_id=$1 AND id=$2")
            .bind(repo)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn list(
    pool: &PgPool,
    repo: i64,
    filter: &Filter,
    offset: i64,
    limit: i64,
) -> Result<Vec<Deployment>> {
    Ok(sqlx::query_as("SELECT * FROM repository_deployments WHERE repo_id=$1 AND (CAST($2 AS TEXT) IS NULL OR sha=$2) AND (CAST($3 AS TEXT) IS NULL OR reference=$3) AND (CAST($4 AS TEXT) IS NULL OR task=$4) AND (CAST($5 AS TEXT) IS NULL OR environment=$5) ORDER BY id DESC LIMIT $6 OFFSET $7")
        .bind(repo).bind(&filter.sha).bind(&filter.reference).bind(&filter.task).bind(&filter.environment).bind(limit).bind(offset).fetch_all(pool).await?)
}

pub async fn delete(pool: &PgPool, actor: i64, repo: i64, id: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        super::discussions::lock_exclusive_write_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repository_deployments WHERE repo_id=$1 AND id=$2)",
    )
    .bind(repo)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(exists, Refusal::NotFound);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM repository_deployments WHERE repo_id=$1")
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
    let state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM deployment_statuses WHERE deployment_id=$1 ORDER BY id DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    anyhow::ensure!(
        count == 1
            || state.is_none_or(|state| matches!(state.as_str(), "inactive" | "error" | "failure")),
        Refusal::Active
    );
    sqlx::query("DELETE FROM repository_deployments WHERE repo_id=$1 AND id=$2")
        .bind(repo)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn create_status(
    pool: &PgPool,
    actor: i64,
    repo: i64,
    deployment: i64,
    input: &NewStatus,
) -> Result<(Deployment, Vec<Status>)> {
    input.validate()?;
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        super::discussions::lock_exclusive_write_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    let current: Deployment =
        sqlx::query_as("SELECT * FROM repository_deployments WHERE repo_id=$1 AND id=$2")
            .bind(repo)
            .bind(deployment)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Refusal::NotFound)?;
    let environment = input.environment.as_deref().unwrap_or(&current.environment);
    let mut saved = Vec::new();
    if input.state == "success" && input.auto_inactive.unwrap_or(true) {
        let older: Vec<Status> = sqlx::query_as("SELECT s.* FROM deployment_statuses s JOIN repository_deployments d ON d.id=s.deployment_id WHERE d.repo_id=$1 AND d.id<$2 AND NOT d.transient_environment AND NOT d.production_environment AND d.environment=$3 AND s.state='success' AND s.id=(SELECT MAX(latest.id) FROM deployment_statuses latest WHERE latest.deployment_id=d.id)")
            .bind(repo).bind(deployment).bind(environment).fetch_all(&mut *tx).await?;
        for status in older {
            let row = sqlx::query_as("INSERT INTO deployment_statuses(deployment_id,creator_id,state,description,environment,log_url,environment_url) VALUES($1,$2,'inactive','',$3,$4,$5) RETURNING *").bind(status.deployment_id).bind(actor).bind(&status.environment).bind(&status.log_url).bind(&status.environment_url).fetch_one(&mut *tx).await?;
            sqlx::query("UPDATE repository_deployments SET updated_at=NOW() WHERE id=$1")
                .bind(status.deployment_id)
                .execute(&mut *tx)
                .await?;
            saved.push(row);
        }
    }
    let log_url = input
        .log_url
        .as_deref()
        .or(input.target_url.as_deref())
        .unwrap_or("");
    let row = sqlx::query_as("INSERT INTO deployment_statuses(deployment_id,creator_id,state,description,environment,log_url,environment_url) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING *")
        .bind(deployment).bind(actor).bind(&input.state).bind(&input.description).bind(environment).bind(log_url).bind(&input.environment_url).fetch_one(&mut *tx).await?;
    saved.push(row);
    sqlx::query("INSERT INTO deployment_environments(repo_id,name,name_key) VALUES($1,$2,$3) ON CONFLICT(repo_id,name_key) DO NOTHING").bind(repo).bind(environment).bind(environment.to_lowercase()).execute(&mut *tx).await?;
    let updated = sqlx::query_as("UPDATE repository_deployments SET environment=$3,updated_at=NOW() WHERE repo_id=$1 AND id=$2 RETURNING *").bind(repo).bind(deployment).bind(environment).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok((updated, saved))
}

pub async fn get_status(pool: &PgPool, deployment: i64, id: i64) -> Result<Option<Status>> {
    Ok(
        sqlx::query_as("SELECT * FROM deployment_statuses WHERE deployment_id=$1 AND id=$2")
            .bind(deployment)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn statuses(
    pool: &PgPool,
    deployment: i64,
    offset: i64,
    limit: i64,
) -> Result<Vec<Status>> {
    Ok(sqlx::query_as("SELECT * FROM deployment_statuses WHERE deployment_id=$1 ORDER BY id DESC LIMIT $2 OFFSET $3").bind(deployment).bind(limit).bind(offset).fetch_all(pool).await?)
}
