use crate::backend::db::secrets::SecretsError;
use sqlx::PgPool;

use crate::backend::crypto::SecretEncryptor;
use crate::backend::db::models::{RepoSecret, RepoSecretMeta};

pub async fn create_or_update_secret(
    pool: &PgPool,
    encryptor: &SecretEncryptor,
    repo_id: i64,
    name: &str,
    value: &str,
    created_by_id: Option<i64>,
) -> Result<(), SecretsError> {
    let actor_id = created_by_id.ok_or(SecretsError::Denied)?;
    let (encrypted_value, nonce) = encryptor.encrypt(value.as_bytes())?;
    let mut tx = pool.begin().await?;
    // The repository row is locked exclusively, so two writes cannot both count the same
    // number of secrets and together pass the limit.
    if !super::discussions::lock_repository_mutation_authority(&mut tx, actor_id, repo_id)
        .await
        .map_err(SecretsError::authority)?
    {
        return Err(SecretsError::Denied);
    }
    // A set larger than one CI claim can carry is refused here, so claims never load more.
    let (others, stored_bytes): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(octet_length(name) + octet_length(encrypted_value)), 0)::BIGINT \
         FROM repo_secrets WHERE repo_id = $1 AND name <> $2",
    )
    .bind(repo_id)
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    crate::backend::db::secrets::ensure_room_for_secret(others, stored_bytes, name, value)?;

    sqlx::query(
        r#"
        INSERT INTO repo_secrets (repo_id, name, encrypted_value, nonce, created_by_id)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (repo_id, name) DO UPDATE SET
            encrypted_value = EXCLUDED.encrypted_value,
            nonce = EXCLUDED.nonce,
            updated_at = NOW()
        "#,
    )
    .bind(repo_id)
    .bind(name)
    .bind(&encrypted_value)
    .bind(&nonce[..])
    .bind(Some(actor_id))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(())
}

pub async fn list_secrets(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
) -> Result<Vec<RepoSecretMeta>, SecretsError> {
    let mut tx = pool.begin().await?;
    if !super::discussions::lock_admin_authority(&mut tx, actor_id, repo_id)
        .await
        .map_err(SecretsError::authority)?
    {
        return Err(SecretsError::Denied);
    }
    let secrets = sqlx::query_as::<
        _,
        (
            String,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        "SELECT name, created_at, updated_at FROM repo_secrets WHERE repo_id = $1 ORDER BY name",
    )
    .bind(repo_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(secrets
        .into_iter()
        .map(|(name, created_at, updated_at)| RepoSecretMeta {
            name,
            created_at,
            updated_at,
        })
        .collect())
}

pub async fn delete_secret(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    name: &str,
) -> Result<bool, SecretsError> {
    let mut tx = pool.begin().await?;
    if !super::discussions::lock_admin_authority(&mut tx, actor_id, repo_id)
        .await
        .map_err(SecretsError::authority)?
    {
        return Err(SecretsError::Denied);
    }
    let result = sqlx::query("DELETE FROM repo_secrets WHERE repo_id = $1 AND name = $2")
        .bind(repo_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(result.rows_affected() > 0)
}

pub async fn decrypt_repository_secrets(
    pool: &PgPool,
    encryptor: &SecretEncryptor,
    repo_id: i64,
) -> Result<Vec<(String, String)>, SecretsError> {
    let secrets = sqlx::query_as::<_, RepoSecret>("SELECT * FROM repo_secrets WHERE repo_id = $1")
        .bind(repo_id)
        .fetch_all(pool)
        .await?;

    let mut result = Vec::with_capacity(secrets.len());

    for secret in secrets {
        let decrypted = encryptor.decrypt(&secret.encrypted_value, &secret.nonce)?;
        let value = String::from_utf8_lossy(&decrypted).to_string();
        result.push((secret.name.clone(), value));
    }

    Ok(result)
}
