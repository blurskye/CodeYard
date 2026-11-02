use super::super::variables::{ensure_room, validate, Variable, VariableError};
use sqlx::PgPool;

pub async fn list(pool: &PgPool, actor: i64, repo: i64) -> Result<Vec<Variable>, VariableError> {
    let mut tx = pool.begin().await?;
    if !super::discussions::lock_settings_read_authority(&mut tx, actor, repo, false)
        .await
        .map_err(VariableError::Authority)?
    {
        return Err(VariableError::Denied);
    }
    let variables = sqlx::query_as("SELECT name, value, created_at, updated_at FROM repo_actions_variables WHERE repo_id=$1 ORDER BY name")
        .bind(repo).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(variables)
}

pub async fn save(
    pool: &PgPool,
    actor: i64,
    repo: i64,
    previous: Option<&str>,
    name: &str,
    value: Option<&str>,
) -> Result<(), VariableError> {
    validate(name, value.unwrap_or_default())?;
    let mut tx = pool.begin().await?;
    if !super::discussions::lock_repository_mutation_authority(&mut tx, actor, repo)
        .await
        .map_err(VariableError::Authority)?
    {
        return Err(VariableError::Denied);
    }
    let old: Option<String> = if let Some(previous) = previous {
        Some(
            sqlx::query_scalar(
                "SELECT value FROM repo_actions_variables WHERE repo_id=$1 AND name=$2",
            )
            .bind(repo)
            .bind(previous)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(VariableError::NotFound)?,
        )
    } else {
        None
    };
    let value = value.or(old.as_deref()).unwrap_or_default();
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repo_actions_variables WHERE repo_id=$1 AND name=$2)",
    )
    .bind(repo)
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    if exists && previous != Some(name) {
        return Err(VariableError::Exists);
    }
    let (count, bytes): (i64, i64) = sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(octet_length(name)+octet_length(value)),0)::BIGINT FROM repo_actions_variables WHERE repo_id=$1 AND ($2::text IS NULL OR name<>$2)")
        .bind(repo).bind(previous).fetch_one(&mut *tx).await?;
    ensure_room(count, bytes, name, value)?;
    if let Some(previous) = previous {
        sqlx::query("UPDATE repo_actions_variables SET name=$3, value=$4, updated_at=clock_timestamp() WHERE repo_id=$1 AND name=$2")
            .bind(repo).bind(previous).bind(name).bind(value).execute(&mut *tx).await?;
    } else {
        sqlx::query("INSERT INTO repo_actions_variables(repo_id,name,value) VALUES($1,$2,$3)")
            .bind(repo)
            .bind(name)
            .bind(value)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn delete(pool: &PgPool, actor: i64, repo: i64, name: &str) -> Result<(), VariableError> {
    let mut tx = pool.begin().await?;
    if !super::discussions::lock_admin_authority(&mut tx, actor, repo)
        .await
        .map_err(VariableError::Authority)?
    {
        return Err(VariableError::Denied);
    }
    let result = sqlx::query("DELETE FROM repo_actions_variables WHERE repo_id=$1 AND name=$2")
        .bind(repo)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(VariableError::NotFound);
    }
    tx.commit().await?;
    Ok(())
}

pub async fn for_run(pool: &PgPool, repo: i64) -> Result<Vec<(String, String)>, VariableError> {
    Ok(sqlx::query_as(
        "SELECT name, value FROM repo_actions_variables WHERE repo_id=$1 ORDER BY name",
    )
    .bind(repo)
    .fetch_all(pool)
    .await?)
}
