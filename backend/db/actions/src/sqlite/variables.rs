use crate::variables::ensure_room;
use crate::variables::validate;
use crate::variables::Variable;
use crate::variables::VariableError;
use codeyard_db_core::pool::SqlitePool;

/// Collaborator access, leaving out the read access a public repository gives everyone: as on
/// GitHub, only collaborators read Actions variables.
pub(crate) async fn is_collaborator(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: i64,
    repo: i64,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM (SELECT id,owner_kind,owner_user_id,owner_org_id,is_gist,TRUE AS is_private FROM repositories WHERE id=?2) r JOIN users u ON u.id=?1 WHERE NOT u.is_banned AND NOT u.must_change_password AND ((r.owner_kind='user' AND u.is_admin) OR ({})>=1))", codeyard_db_core::queries::sqlite::repositories::access::USER_REPO_ACCESS_SQL))
        .bind(actor)
        .bind(repo)
        .fetch_one(&mut **tx)
        .await
}

async fn credential_authority(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    repo: i64,
    write: bool,
) -> Result<(), VariableError> {
    use codeyard_db_repos::repositories::credentials::repository_condition;
    use codeyard_db_repos::repositories::credentials::Access;
    use codeyard_db_repos::repositories::credentials::Reader;
    if !codeyard_db_core::queries::sqlite::users::transitions::credential_live(
        tx, actor, credential, write,
    )
    .await
    .map_err(VariableError::Authority)?
    {
        return Err(VariableError::Denied);
    }
    let reader = Reader::authenticated(actor, credential);
    let access = if write { Access::Write } else { Access::Read };
    let allowed: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=?1 WHERE r.id=?2 AND ({}))",
        repository_condition(false, "variables", access, "?3", "?4", "?5"),
    ))
    .bind(actor).bind(repo).bind(reader.token()).bind(reader.session()).bind(reader.hash())
    .fetch_one(&mut **tx).await?;
    if !allowed {
        return Err(VariableError::Denied);
    }
    Ok(())
}

pub async fn list(
    pool: &SqlitePool,
    actor: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    repo: i64,
) -> Result<Vec<Variable>, VariableError> {
    let mut tx = pool.begin().await?;
    let collaborator = is_collaborator(&mut tx, actor, repo).await?;
    if !collaborator {
        return Err(VariableError::Denied);
    }
    credential_authority(&mut tx, actor, credential, repo, false).await?;
    let variables = sqlx::query_as("SELECT name, value, created_at, updated_at FROM repo_actions_variables WHERE repo_id=?1 ORDER BY name")
        .bind(repo).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(variables)
}

pub async fn save(
    pool: &SqlitePool,
    actor: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    repo: i64,
    previous: Option<&str>,
    name: &str,
    value: Option<&str>,
) -> Result<(), VariableError> {
    validate(name, value.unwrap_or_default())?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !codeyard_db_core::queries::sqlite::discussions::lock_write_authority(&mut tx, actor, repo)
        .await
        .map_err(VariableError::Authority)?
    {
        return Err(VariableError::Denied);
    }
    credential_authority(&mut tx, actor, credential, repo, true).await?;
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
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    repo: i64,
    name: &str,
) -> Result<(), VariableError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !codeyard_db_core::queries::sqlite::discussions::lock_write_authority(&mut tx, actor, repo)
        .await
        .map_err(VariableError::Authority)?
    {
        return Err(VariableError::Denied);
    }
    credential_authority(&mut tx, actor, credential, repo, true).await?;
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
