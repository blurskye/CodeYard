use super::super::environment_values::{
    self, EnvironmentValue, Kind, Save, StoredValue, ValueError,
};
use crate::backend::crypto::{SecretEncryptor, SecretError};
use sqlx::{PgConnection as Connection, PgPool as Pool};

async fn authorize(
    connection: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: i64,
    repo: i64,
    admin: bool,
) -> Result<(), ValueError> {
    let allowed = if admin {
        super::discussions::lock_admin_authority(connection, actor, repo).await
    } else {
        super::discussions::lock_contribution_authority(connection, actor, repo).await
    }
    .map_err(ValueError::Authority)?;
    if allowed {
        Ok(())
    } else {
        Err(ValueError::Denied)
    }
}

/// Reads stay available on archived repositories, unlike changes.
async fn authorize_read(
    connection: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: i64,
    repo: i64,
    admin: bool,
) -> Result<(), ValueError> {
    if super::discussions::lock_settings_read_authority(connection, actor, repo, admin)
        .await
        .map_err(ValueError::Authority)?
    {
        Ok(())
    } else {
        Err(ValueError::Denied)
    }
}

async fn require_environment(
    connection: &mut Connection,
    repo: i64,
    environment: &str,
) -> Result<(), ValueError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM deployment_environments WHERE repo_id=$1 AND name_key=$2)",
    )
    .bind(repo)
    .bind(environment)
    .fetch_one(connection)
    .await?;
    if exists {
        Ok(())
    } else {
        Err(ValueError::NotFound)
    }
}

// Repository authority locks are shared. Serialize scoped writes before reading prior
// values or aggregate budgets, including inserts of two different names.
async fn lock_environment(
    connection: &mut Connection,
    repo: i64,
    environment: &str,
) -> Result<(), ValueError> {
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM deployment_environments WHERE repo_id=$1 AND name_key=$2 FOR UPDATE",
    )
    .bind(repo)
    .bind(environment)
    .fetch_optional(connection)
    .await?;
    id.map(|_| ()).ok_or(ValueError::NotFound)
}

pub async fn create(pool: &Pool, actor: i64, repo: i64, name: &str) -> Result<(), ValueError> {
    let key = environment_values::environment_key(name)?;
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, repo, true).await?;
    sqlx::query("INSERT INTO deployment_environments(repo_id,name_key,name) VALUES($1,$2,$3) ON CONFLICT(repo_id,name_key) DO NOTHING").bind(repo).bind(key).bind(name).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn environments(pool: &Pool, actor: i64, repo: i64) -> Result<Vec<String>, ValueError> {
    let mut tx = pool.begin().await?;
    authorize_read(&mut tx, actor, repo, true).await?;
    let names = sqlx::query_scalar(
        "SELECT name FROM deployment_environments WHERE repo_id=$1 ORDER BY name_key",
    )
    .bind(repo)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(names)
}

pub async fn list(
    pool: &Pool,
    actor: i64,
    repo: i64,
    environment: &str,
    kind: Kind,
) -> Result<Vec<EnvironmentValue>, ValueError> {
    let key = environment_values::environment_key(environment)?;
    let mut tx = pool.begin().await?;
    authorize_read(&mut tx, actor, repo, kind == Kind::Secret).await?;
    require_environment(&mut tx, repo, &key).await?;
    let rows = sqlx::query_as("SELECT name,value,created_at,updated_at FROM repo_environment_values WHERE repo_id=$1 AND environment=$2 AND kind=$3 ORDER BY name").bind(repo).bind(key).bind(kind.as_str()).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn save(
    pool: &Pool,
    encryptor: Option<&SecretEncryptor>,
    actor: i64,
    repo: i64,
    environment: &str,
    input: &Save<'_>,
) -> Result<bool, ValueError> {
    let key = environment_values::environment_key(environment)?;
    environment_values::validate(input.name, input.value.unwrap_or_default())?;
    let name = input.name.to_ascii_uppercase();
    let previous = input.previous.unwrap_or(input.name).to_ascii_uppercase();
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, repo, true).await?;
    lock_environment(&mut tx, repo, &key).await?;
    let old: Option<String> = sqlx::query_scalar("SELECT value FROM repo_environment_values WHERE repo_id=$1 AND environment=$2 AND kind=$3 AND name=$4").bind(repo).bind(&key).bind(input.kind.as_str()).bind(&previous).fetch_optional(&mut *tx).await?;
    if input.kind == Kind::Variable {
        if input.previous.is_some() && old.is_none() {
            return Err(ValueError::NotFound);
        }
        if input.previous.is_none() && old.is_some() {
            return Err(ValueError::Exists);
        }
    }
    if name != previous {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repo_environment_values WHERE repo_id=$1 AND environment=$2 AND kind=$3 AND name=$4)").bind(repo).bind(&key).bind(input.kind.as_str()).bind(&name).fetch_one(&mut *tx).await?;
        if exists {
            return Err(ValueError::Exists);
        }
    }
    let value = input
        .value
        .or(old.as_deref())
        .ok_or(ValueError::InvalidValue)?;
    let (count, bytes): (i64, i64) = sqlx::query_as("SELECT COUNT(*),COALESCE(SUM(length(name)+COALESCE(octet_length(encrypted_value),octet_length(value))),0) FROM repo_environment_values WHERE repo_id=$1 AND environment=$2 AND kind=$3 AND name<>$4").bind(repo).bind(&key).bind(input.kind.as_str()).bind(&previous).fetch_one(&mut *tx).await?;
    if count >= codeyard_actions::values::MAX_SCOPE_VALUES as i64 {
        return Err(ValueError::Limit);
    }
    if input.kind == Kind::Secret {
        super::super::secrets::ensure_room_for_secret(count, bytes, &name, value)
            .map_err(|_| ValueError::SecretsSize)?;
    }
    if input.kind == Kind::Variable && bytes + (name.len() + value.len()) as i64 > codeyard_actions::values::MAX_VARIABLE_BYTES as i64 {
        return Err(ValueError::Size);
    }
    let (plain, encrypted, nonce) = if input.kind == Kind::Secret {
        let (encrypted, nonce) = encryptor
            .ok_or(SecretError::InvalidMasterKey)?
            .encrypt(value.as_bytes())?;
        ("", Some(encrypted), Some(nonce.to_vec()))
    } else {
        (value, None, None)
    };
    if name != previous {
        sqlx::query("UPDATE repo_environment_values SET name=$5 WHERE repo_id=$1 AND environment=$2 AND kind=$3 AND name=$4").bind(repo).bind(&key).bind(input.kind.as_str()).bind(&previous).bind(&name).execute(&mut *tx).await?;
    }
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO repo_environment_values(repo_id,environment,kind,name,value,encrypted_value,nonce,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$8) ON CONFLICT(repo_id,environment,kind,name) DO UPDATE SET value=excluded.value,encrypted_value=excluded.encrypted_value,nonce=excluded.nonce,updated_at=excluded.updated_at")
        .bind(repo).bind(key).bind(input.kind.as_str()).bind(name).bind(plain).bind(encrypted).bind(nonce).bind(now).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(old.is_none())
}

pub async fn delete(
    pool: &Pool,
    actor: i64,
    repo: i64,
    environment: &str,
    kind: Kind,
    name: &str,
) -> Result<(), ValueError> {
    let key = environment_values::environment_key(environment)?;
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, repo, true).await?;
    lock_environment(&mut tx, repo, &key).await?;
    let result = sqlx::query("DELETE FROM repo_environment_values WHERE repo_id=$1 AND environment=$2 AND kind=$3 AND name=$4").bind(repo).bind(key).bind(kind.as_str()).bind(name.to_ascii_uppercase()).execute(&mut *tx).await?;
    if result.rows_affected() == 0 {
        return Err(ValueError::NotFound);
    }
    tx.commit().await?;
    Ok(())
}

pub async fn workflow_values(
    pool: &Pool,
    repo: i64,
    environment: &str,
    kind: Kind,
) -> Result<Vec<StoredValue>, ValueError> {
    let key = environment_values::environment_key(environment)?;
    Ok(sqlx::query_as("SELECT name,value,encrypted_value,nonce FROM repo_environment_values WHERE repo_id=$1 AND environment=$2 AND kind=$3 ORDER BY name").bind(repo).bind(key).bind(kind.as_str()).fetch_all(pool).await?)
}
