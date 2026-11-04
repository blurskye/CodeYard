use crate::backend::db::variables::ensure_room;
use crate::backend::db::variables::validate;
use crate::backend::db::variables::Variable;
use crate::backend::db::variables::VariableError;
use sqlx::PgPool;

/// Collaborator access, leaving out the read access a public repository gives everyone: as on
/// GitHub, only collaborators read Actions variables.
pub(crate) async fn is_collaborator(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: i64,
    repo: i64,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM (SELECT id,owner_kind,owner_user_id,owner_org_id,is_gist,TRUE AS is_private FROM repositories WHERE id=$2) r JOIN users u ON u.id=$1 WHERE NOT u.is_banned AND NOT u.must_change_password AND ((r.owner_kind='user' AND u.is_admin) OR ({})>=1))", crate::backend::db::postgres::repositories::access::USER_REPO_ACCESS_SQL))
        .bind(actor)
        .bind(repo)
        .fetch_one(&mut **tx)
        .await
}

// Repository/account authority is already retained; do not upgrade the account lock here.
async fn credential_authority(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    repo: i64,
    write: bool,
) -> Result<(), VariableError> {
    use crate::backend::db::repositories::credentials::repository_condition;
    use crate::backend::db::repositories::credentials::Access;
    use crate::backend::db::repositories::credentials::Reader;
    if !crate::backend::db::postgres::users::transitions::credential_live(
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
        "SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=$1 WHERE r.id=$2 AND ({}))",
        repository_condition(true, "variables", access, "$3", "$4", "$5"),
    ))
    .bind(actor).bind(repo).bind(reader.token()).bind(reader.session()).bind(reader.hash())
    .fetch_one(&mut **tx).await?;
    if !allowed {
        return Err(VariableError::Denied);
    }
    Ok(())
}

pub async fn list(
    pool: &PgPool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    repo: i64,
) -> Result<Vec<Variable>, VariableError> {
    let mut tx = pool.begin().await?;
    if crate::backend::db::postgres::repositories::access::lock_user_repo_access_in_transaction(
        &mut tx,
        actor,
        repo,
        crate::backend::db::postgres::repositories::access::RepositoryRowLock::Shared,
        true,
    )
    .await
    .map_err(VariableError::Authority)?
    .is_none()
    {
        return Err(VariableError::Denied);
    }
    // Suppress visibility-only access while retaining the canonical grant calculation.
    let collaborator = is_collaborator(&mut tx, actor, repo).await?;
    if !collaborator {
        return Err(VariableError::Denied);
    }
    credential_authority(&mut tx, actor, credential, repo, false).await?;
    let variables = sqlx::query_as("SELECT name, value, created_at, updated_at FROM repo_actions_variables WHERE repo_id=$1 ORDER BY name")
        .bind(repo).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(variables)
}

pub async fn save(
    pool: &PgPool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    repo: i64,
    previous: Option<&str>,
    name: &str,
    value: Option<&str>,
) -> Result<(), VariableError> {
    validate(name, value.unwrap_or_default())?;
    let mut tx = pool.begin().await?;
    if !crate::backend::db::postgres::discussions::lock_exclusive_write_authority(
        &mut tx, actor, repo,
    )
    .await
    .map_err(VariableError::Authority)?
    {
        return Err(VariableError::Denied);
    }
    credential_authority(&mut tx, actor, credential, repo, true).await?;
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

pub async fn delete(
    pool: &PgPool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    repo: i64,
    name: &str,
) -> Result<(), VariableError> {
    let mut tx = pool.begin().await?;
    if !crate::backend::db::postgres::discussions::lock_exclusive_write_authority(
        &mut tx, actor, repo,
    )
    .await
    .map_err(VariableError::Authority)?
    {
        return Err(VariableError::Denied);
    }
    credential_authority(&mut tx, actor, credential, repo, true).await?;
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
