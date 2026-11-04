use crate::deployments::pair;
use crate::deployments::Deployment;
use crate::deployments::Filter;
use crate::deployments::NewDeployment;
use crate::deployments::NewStatus;
use crate::deployments::Refusal;
use crate::deployments::Status;
use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::queries::credentials::Actor;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;

async fn write<'p>(
    pool: &'p SqlitePool,
    actor: &Actor,
    repo: i64,
) -> Result<Transaction<'p, Sqlite>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_write_authority(
            &mut tx,
            actor.user_id,
            repo
        )
        .await?
            && codeyard_db_core::queries::sqlite::credentials::repository(
                &mut tx,
                actor,
                repo,
                "deployments",
                true
            )
            .await?,
        Refusal::Denied
    );
    Ok(tx)
}

pub async fn create(pool: &SqlitePool, input: NewDeployment<'_>) -> Result<Deployment> {
    input.validate()?;
    let mut tx = write(pool, &input.actor, input.repo).await?;
    let checks = sqlx::query_as(
        "SELECT context,state FROM commit_statuses_latest WHERE repo_id=?1 AND sha=?2",
    )
    .bind(input.repo)
    .bind(input.sha)
    .fetch_all(&mut *tx)
    .await?;
    ensure!(input.checks_passed(&checks), Refusal::Checks);
    let key = input.environment.to_lowercase();
    sqlx::query("INSERT INTO deployment_environments(repo_id,name,name_key) VALUES(?1,?2,?3) ON CONFLICT(repo_id,name_key) DO NOTHING")
        .bind(input.repo).bind(input.environment).bind(&key).execute(&mut *tx).await?;
    let row = sqlx::query_as("INSERT INTO repository_deployments(repo_id,creator_id,sha,reference,task,payload,original_environment,environment,description,transient_environment,production_environment,environment_key) VALUES(?1,?2,?3,?4,?5,?6,?7,?7,?8,?9,?10,?11) RETURNING *")
        .bind(input.repo).bind(input.actor.user_id).bind(input.sha).bind(input.reference).bind(input.task).bind(input.payload.to_string()).bind(input.environment).bind(input.description).bind(input.transient).bind(input.production).bind(key).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}

pub async fn get(
    pool: &SqlitePool,
    viewer: Option<i64>,
    repo: i64,
    id: i64,
) -> Result<Option<Deployment>> {
    Ok(sqlx::query_as(&format!("SELECT d.* FROM repository_deployments d JOIN repositories r ON r.id=d.repo_id LEFT JOIN users u ON u.id=?3 WHERE d.repo_id=?1 AND d.id=?2 AND {}", codeyard_db_core::queries::sqlite::repositories::access::readable_condition()))
        .bind(repo).bind(id).bind(viewer).fetch_optional(pool).await?)
}

pub async fn list(
    pool: &SqlitePool,
    viewer: Option<i64>,
    repo: i64,
    filter: &Filter,
    offset: i64,
    limit: i64,
) -> Result<Vec<Deployment>> {
    Ok(sqlx::query_as(&format!("SELECT d.* FROM repository_deployments d JOIN repositories r ON r.id=d.repo_id LEFT JOIN users u ON u.id=?8 WHERE d.repo_id=?1 AND (?2 IS NULL OR d.sha=?2) AND (?3 IS NULL OR d.reference=?3) AND (?4 IS NULL OR d.task=?4) AND (?5 IS NULL OR d.environment_key=?5) AND {} ORDER BY d.id DESC LIMIT ?6 OFFSET ?7", codeyard_db_core::queries::sqlite::repositories::access::readable_condition()))
        .bind(repo).bind(&filter.sha).bind(&filter.reference).bind(&filter.task).bind(filter.environment.as_ref().map(|name| name.to_lowercase())).bind(limit).bind(offset).bind(viewer).fetch_all(pool).await?)
}

pub async fn delete(pool: &SqlitePool, actor: impl Into<Actor>, repo: i64, id: i64) -> Result<()> {
    let actor = actor.into();
    let mut tx = write(pool, &actor, repo).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repository_deployments WHERE repo_id=?1 AND id=?2)",
    )
    .bind(repo)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    ensure!(exists, Refusal::NotFound);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM repository_deployments WHERE repo_id=?1")
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
    let state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM deployment_statuses WHERE deployment_id=?1 ORDER BY id DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    ensure!(
        count == 1
            || state.is_none_or(|state| matches!(state.as_str(), "inactive" | "error" | "failure")),
        Refusal::Active
    );
    sqlx::query("DELETE FROM repository_deployments WHERE repo_id=?1 AND id=?2")
        .bind(repo)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn create_status(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    repo: i64,
    deployment: i64,
    input: &NewStatus,
) -> Result<Vec<(Deployment, Status)>> {
    input.validate()?;
    let actor = actor.into();
    let mut tx = write(pool, &actor, repo).await?;
    let current: Deployment =
        sqlx::query_as("SELECT * FROM repository_deployments WHERE repo_id=?1 AND id=?2")
            .bind(repo)
            .bind(deployment)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Refusal::NotFound)?;
    let environment = input.environment.as_deref().unwrap_or(&current.environment);
    let key = environment.to_lowercase();
    // Each status is returned with its deployment as this transaction left it.
    let mut saved: Vec<(Deployment, Status)> = Vec::new();
    if input.state == "success" && input.auto_inactive.unwrap_or(true) {
        let statuses: Vec<Status> = sqlx::query_as("INSERT INTO deployment_statuses(deployment_id,creator_id,state,description,environment,log_url,environment_url) SELECT d.id,?4,'inactive','',s.environment,s.log_url,s.environment_url FROM deployment_statuses s JOIN repository_deployments d ON d.id=s.deployment_id WHERE d.repo_id=?1 AND d.id<?2 AND NOT d.transient_environment AND NOT d.production_environment AND d.environment_key=?3 AND s.state='success' AND s.id=(SELECT MAX(latest.id) FROM deployment_statuses latest WHERE latest.deployment_id=d.id) ORDER BY d.id RETURNING *")
            .bind(repo).bind(deployment).bind(&key).bind(actor.user_id).fetch_all(&mut *tx).await?;
        if !statuses.is_empty() {
            let ids: Vec<_> = statuses.iter().map(|status| status.deployment_id).collect();
            let deployments: Vec<Deployment> = sqlx::query_as("UPDATE repository_deployments SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id IN (SELECT value FROM json_each(?1)) RETURNING *")
                .bind(serde_json::to_string(&ids)?).fetch_all(&mut *tx).await?;
            saved = pair(deployments, statuses)?;
        }
    }
    let log_url = input
        .log_url
        .as_deref()
        .or(input.target_url.as_deref())
        .unwrap_or("");
    let row = sqlx::query_as("INSERT INTO deployment_statuses(deployment_id,creator_id,state,description,environment,log_url,environment_url) VALUES(?1,?2,?3,?4,?5,?6,?7) RETURNING *")
        .bind(deployment).bind(actor.user_id).bind(&input.state).bind(&input.description).bind(environment).bind(log_url).bind(&input.environment_url).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO deployment_environments(repo_id,name,name_key) VALUES(?1,?2,?3) ON CONFLICT(repo_id,name_key) DO NOTHING")
        .bind(repo).bind(environment).bind(&key).execute(&mut *tx).await?;
    let updated = sqlx::query_as("UPDATE repository_deployments SET environment=?3,environment_key=?4,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE repo_id=?1 AND id=?2 RETURNING *")
        .bind(repo).bind(deployment).bind(environment).bind(key).fetch_one(&mut *tx).await?;
    saved.push((updated, row));
    tx.commit().await?;
    Ok(saved)
}

pub async fn get_status(
    pool: &SqlitePool,
    viewer: Option<i64>,
    deployment: i64,
    id: i64,
) -> Result<Option<Status>> {
    Ok(sqlx::query_as(&format!("SELECT s.* FROM deployment_statuses s JOIN repository_deployments d ON d.id=s.deployment_id JOIN repositories r ON r.id=d.repo_id LEFT JOIN users u ON u.id=?3 WHERE s.deployment_id=?1 AND s.id=?2 AND {}", codeyard_db_core::queries::sqlite::repositories::access::readable_condition()))
        .bind(deployment).bind(id).bind(viewer).fetch_optional(pool).await?)
}

pub async fn statuses(
    pool: &SqlitePool,
    viewer: Option<i64>,
    deployment: i64,
    offset: i64,
    limit: i64,
) -> Result<Vec<Status>> {
    Ok(sqlx::query_as(&format!("SELECT s.* FROM deployment_statuses s JOIN repository_deployments d ON d.id=s.deployment_id JOIN repositories r ON r.id=d.repo_id LEFT JOIN users u ON u.id=?4 WHERE s.deployment_id=?1 AND {} ORDER BY s.id DESC LIMIT ?2 OFFSET ?3", codeyard_db_core::queries::sqlite::repositories::access::readable_condition()))
        .bind(deployment).bind(limit).bind(offset).bind(viewer).fetch_all(pool).await?)
}
