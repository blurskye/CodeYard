use crate::backend::db::{
    action_workflows::Workflow, models::CiRun, repositories::records::RepositorySettingsRefusal,
};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn register_workflow(
    pool: &SqlitePool,
    repo: i64,
    path: &str,
    name: &str,
    refresh: bool,
) -> Result<Workflow> {
    Ok(sqlx::query_as("INSERT INTO actions_workflows(repo_id,path,name) VALUES($1,$2,$3) ON CONFLICT(repo_id,path) DO UPDATE SET name=CASE WHEN $4 THEN excluded.name ELSE actions_workflows.name END,updated_at=CASE WHEN $4 AND actions_workflows.name<>excluded.name THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now') ELSE actions_workflows.updated_at END RETURNING *")
        .bind(repo).bind(path).bind(name).bind(refresh).fetch_one(pool).await?)
}

pub async fn workflow_by_id(pool: &SqlitePool, repo: i64, id: i64) -> Result<Option<Workflow>> {
    Ok(
        sqlx::query_as("SELECT * FROM actions_workflows WHERE repo_id=$1 AND id=$2")
            .bind(repo)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn workflow_for_path(
    pool: &SqlitePool,
    repo: i64,
    path: &str,
) -> Result<Option<Workflow>> {
    Ok(
        sqlx::query_as("SELECT * FROM actions_workflows WHERE repo_id=$1 AND path=$2")
            .bind(repo)
            .bind(path)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn set_workflow_enabled(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    id: i64,
    enabled: bool,
) -> Result<Option<Workflow>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        super::discussions::lock_write_authority(&mut tx, actor, repo).await?,
        RepositorySettingsRefusal::Denied
    );
    let workflow=sqlx::query_as("UPDATE actions_workflows SET state=$3,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE repo_id=$1 AND id=$2 RETURNING *").bind(repo).bind(id).bind(if enabled {"active"} else {"disabled_manually"}).fetch_optional(&mut *tx).await?;
    if workflow.is_some() {
        sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,'repository.actions_workflow','repository',$2,$3)").bind(actor).bind(repo).bind(serde_json::json!({"workflow_id":id,"enabled":enabled})).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(workflow)
}

pub async fn latest_workflow_run(
    pool: &SqlitePool,
    repo: i64,
    path: &str,
    branch: Option<&str>,
    event: Option<&str>,
) -> Result<Option<CiRun>> {
    Ok(sqlx::query_as("SELECT * FROM ci_runs WHERE repo_id=$1 AND instr(','||workflow_path||',',','||$2||',')>0 AND ($3 IS NULL OR branch=$3) AND ($4 IS NULL OR event=$4) ORDER BY COALESCE(workflow_run_id,id) DESC,id DESC LIMIT 1").bind(repo).bind(path).bind(branch).bind(event).fetch_optional(pool).await?)
}
