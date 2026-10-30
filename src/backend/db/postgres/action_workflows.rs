use crate::backend::db::{
    action_workflows::Workflow, models::CiRun, repositories::records::RepositorySettingsRefusal,
};
use anyhow::Result;
use sqlx::PgPool;

pub async fn register_workflows(pool: &PgPool, repo: i64, definitions: &[(String, String)], refresh: bool) -> Result<Vec<Workflow>> {
    let paths: Vec<_> = definitions.iter().map(|(path, _)| path.clone()).collect();
    let rows: Vec<Workflow> = sqlx::query_as("SELECT * FROM actions_workflows WHERE repo_id=$1 AND path=ANY($2) ORDER BY id")
        .bind(repo).bind(&paths).fetch_all(pool).await?;
    let changes: Vec<_> = definitions.iter().filter(|(path, name)| !rows.iter().any(|row| row.path == *path && (!refresh || row.name == *name))).collect();
    if changes.is_empty() { return Ok(rows) }
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new("INSERT INTO actions_workflows(repo_id,path,name) ");
    query.push_values(changes, |mut row, (path, name)| { row.push_bind(repo).push_bind(path).push_bind(name); });
    query.push(if refresh { " ON CONFLICT(repo_id,path) DO UPDATE SET name=EXCLUDED.name,updated_at=NOW() WHERE actions_workflows.name<>EXCLUDED.name" } else { " ON CONFLICT(repo_id,path) DO NOTHING" });
    query.build().execute(pool).await?;
    Ok(sqlx::query_as("SELECT * FROM actions_workflows WHERE repo_id=$1 AND path=ANY($2) ORDER BY id")
        .bind(repo).bind(&paths).fetch_all(pool).await?)
}

pub async fn workflow_by_id(pool: &PgPool, repo: i64, id: i64) -> Result<Option<Workflow>> {
    Ok(
        sqlx::query_as("SELECT * FROM actions_workflows WHERE repo_id=$1 AND id=$2")
            .bind(repo)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn workflow_for_path(pool: &PgPool, repo: i64, path: &str) -> Result<Option<Workflow>> {
    Ok(
        sqlx::query_as("SELECT * FROM actions_workflows WHERE repo_id=$1 AND path=$2")
            .bind(repo)
            .bind(path)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn set_workflow_enabled(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::users::tokens::Actor>,
    repo: i64,
    id: i64,
    enabled: bool,
) -> Result<Option<Workflow>> {
    let credential = actor.into();
    let actor = credential.user_id;
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        super::discussions::lock_write_authority(&mut tx, actor, repo).await?,
        RepositorySettingsRefusal::Denied
    );
    if let Some(token) = credential.token_id {
        anyhow::ensure!(super::users::tokens::permits_write(&mut tx, actor, token, Some(repo), None, "actions").await?, RepositorySettingsRefusal::Denied);
    }
    let workflow=sqlx::query_as("UPDATE actions_workflows SET state=$3,updated_at=NOW() WHERE repo_id=$1 AND id=$2 RETURNING *").bind(repo).bind(id).bind(if enabled {"active"} else {"disabled_manually"}).fetch_optional(&mut *tx).await?;
    if workflow.is_some() {
        sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,'repository.actions_workflow','repository',$2,$3)").bind(actor).bind(repo).bind(serde_json::json!({"workflow_id":id,"enabled":enabled})).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(workflow)
}

pub async fn latest_workflow_run(
    pool: &PgPool,
    repo: i64,
    path: &str,
    branch: Option<&str>,
    event: Option<&str>,
) -> Result<Option<CiRun>> {
    Ok(sqlx::query_as("SELECT * FROM ci_runs WHERE repo_id=$1 AND strpos(','||workflow_path||',',','||$2||',')>0 AND ($3::text IS NULL OR CASE WHEN event='pull_request' THEN COALESCE(pull_request_event->>'head_ref',branch) ELSE branch END=$3) AND ($4::text IS NULL OR event=$4) ORDER BY COALESCE(workflow_run_id,id) DESC,id DESC LIMIT 1").bind(repo).bind(path).bind(branch).bind(event).fetch_optional(pool).await?)
}
