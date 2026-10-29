use super::super::variables::{ensure_room, validate, Variable, VariableError};
use sqlx::SqlitePool;

pub async fn list(
    pool: &SqlitePool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    repo: i64,
) -> Result<Vec<Variable>, VariableError> {
    let mut tx = pool.begin().await?;
    let collaborator: bool = sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM (SELECT id,owner_kind,owner_user_id,owner_org_id,is_gist,TRUE AS is_private FROM repositories WHERE id=?2) r JOIN users u ON u.id=?1 WHERE NOT u.is_banned AND NOT u.must_change_password AND ((r.owner_kind='user' AND u.is_admin) OR ({})>=1))", super::repositories::access::USER_REPO_ACCESS_SQL))
        .bind(actor).bind(repo).fetch_one(&mut *tx).await?;
    if !collaborator || !super::users::transitions::credential_live(&mut tx, actor, credential, false).await.map_err(VariableError::Authority)? {
        return Err(VariableError::Denied);
    }
    let variables = sqlx::query_as("SELECT name, value, created_at, updated_at FROM repo_actions_variables WHERE repo_id=?1 ORDER BY name")
        .bind(repo).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(variables)
}

pub async fn save(
    pool: &SqlitePool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    repo: i64,
    previous: Option<&str>,
    name: &str,
    value: Option<&str>,
) -> Result<(), VariableError> {
    validate(name, value.unwrap_or_default())?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::users::transitions::require_account(&mut tx, actor, credential).await.map_err(super::super::variables::authority)?;
    if !super::discussions::lock_write_authority(&mut tx, actor, repo)
        .await
        .map_err(VariableError::Authority)?
    {
        return Err(VariableError::Denied);
    }
    let old: Option<String> = if let Some(previous) = previous {
        Some(
            sqlx::query_scalar(
                "SELECT value FROM repo_actions_variables WHERE repo_id=?1 AND name=?2",
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
        "SELECT EXISTS(SELECT 1 FROM repo_actions_variables WHERE repo_id=?1 AND name=?2)",
    )
    .bind(repo)
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    if exists && previous != Some(name) {
        return Err(VariableError::Exists);
    }
    let (count, bytes): (i64, i64) = sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(length(CAST(name AS BLOB))+length(CAST(value AS BLOB))),0) FROM repo_actions_variables WHERE repo_id=?1 AND (?2 IS NULL OR name<>?2)")
        .bind(repo).bind(previous).fetch_one(&mut *tx).await?;
    ensure_room(count, bytes, name, value)?;
    if let Some(previous) = previous {
        sqlx::query("UPDATE repo_actions_variables SET name=?3, value=?4, updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE repo_id=?1 AND name=?2")
            .bind(repo).bind(previous).bind(name).bind(value).execute(&mut *tx).await?;
    } else {
        sqlx::query("INSERT INTO repo_actions_variables(repo_id,name,value) VALUES(?1,?2,?3)")
            .bind(repo)
            .bind(name)
            .bind(value)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn delete(
    pool: &SqlitePool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    repo: i64,
    name: &str,
) -> Result<(), VariableError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::users::transitions::require_account(&mut tx, actor, credential).await.map_err(super::super::variables::authority)?;
    if !super::discussions::lock_write_authority(&mut tx, actor, repo)
        .await
        .map_err(VariableError::Authority)?
    {
        return Err(VariableError::Denied);
    }
    let result = sqlx::query("DELETE FROM repo_actions_variables WHERE repo_id=?1 AND name=?2")
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

pub async fn for_run(pool: &SqlitePool, repo: i64) -> Result<Vec<(String, String)>, VariableError> {
    Ok(sqlx::query_as(
        "SELECT name, value FROM repo_actions_variables WHERE repo_id=?1 ORDER BY name",
    )
    .bind(repo)
    .fetch_all(pool)
    .await?)
}
