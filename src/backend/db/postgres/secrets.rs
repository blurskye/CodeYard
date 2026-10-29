use crate::backend::db::secrets::{SecretKind, SecretsError};
use sqlx::PgPool;

use crate::backend::crypto::SecretEncryptor;
use crate::backend::db::models::{RepoSecret, RepoSecretMeta};

pub async fn create_or_update_secret(
    pool: &PgPool,
    encryptor: &SecretEncryptor,
    (repo_id, kind): (i64, SecretKind),
    name: &str,
    value: &str,
    created_by_id: Option<crate::backend::db::users::accounts::AccountActor>,
) -> Result<bool, SecretsError> {
    let authority = created_by_id.ok_or(SecretsError::Denied)?;
    let actor_id = authority.user_id();
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
    if let Some(credential) = authority.credential() {
        super::users::transitions::require_account(&mut tx, actor_id, &credential).await.map_err(SecretsError::authority)?;
    }
    let table = kind.secrets_table();
    let exists: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM {table} WHERE repo_id = $1 AND name = $2)",
    ))
    .bind(repo_id)
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    // A set larger than one CI claim can carry is refused here, so claims never load more.
    let (others, stored_bytes): (i64, i64) = sqlx::query_as(&format!(
        "SELECT COUNT(*), COALESCE(SUM(octet_length(name) + octet_length(encrypted_value)), 0)::BIGINT \
         FROM {table} WHERE repo_id = $1 AND name <> $2",
    ))
    .bind(repo_id)
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    crate::backend::db::secrets::ensure_room_for_secret(others, stored_bytes, name, value)?;

    sqlx::query(&format!(
        r#"
        INSERT INTO {table} (repo_id, name, encrypted_value, nonce, created_by_id)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (repo_id, name) DO UPDATE SET
            encrypted_value = EXCLUDED.encrypted_value,
            nonce = EXCLUDED.nonce,
            updated_at = NOW()
        "#,
    ))
    .bind(repo_id)
    .bind(name)
    .bind(&encrypted_value)
    .bind(&nonce[..])
    .bind(Some(actor_id))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(!exists)
}

pub async fn repository_secret_key(
    pool: &PgPool,
    kind: SecretKind,
    encryptor: &SecretEncryptor,
    actor_id: i64,
    repo_id: i64,
) -> Result<(String, crypto_box::SecretKey), SecretsError> {
    let mut tx = pool.begin().await?;
    if !super::discussions::lock_repository_mutation_authority(&mut tx, actor_id, repo_id)
        .await
        .map_err(SecretsError::authority)?
    {
        return Err(SecretsError::Denied);
    }
    let key = crypto_box::SecretKey::generate(&mut rand::thread_rng());
    let (encrypted, nonce) = encryptor.encrypt(&key.to_bytes())?;
    let table = kind.keys_table();
    sqlx::query(&format!(
        "INSERT INTO {table} (repo_id, key_id, encrypted_key, nonce) VALUES ($1, $2, $3, $4) ON CONFLICT(repo_id) DO NOTHING",
    ))
    .bind(repo_id)
    .bind(uuid::Uuid::new_v4().simple().to_string())
    .bind(encrypted)
    .bind(nonce.as_slice())
    .execute(&mut *tx)
    .await?;
    let (id, encrypted, nonce): (String, Vec<u8>, Vec<u8>) = sqlx::query_as(&format!(
        "SELECT key_id, encrypted_key, nonce FROM {table} WHERE repo_id = $1",
    ))
    .bind(repo_id)
    .fetch_one(&mut *tx)
    .await?;
    let bytes: [u8; 32] = encryptor
        .decrypt(&encrypted, &nonce)?
        .try_into()
        .map_err(|_| crate::backend::crypto::SecretError::DecryptionFailed)?;
    tx.commit().await?;
    Ok((id, crypto_box::SecretKey::from(bytes)))
}

pub async fn repository_has_secrets(pool: &PgPool, repo_id: i64) -> Result<bool, SecretsError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repo_secrets WHERE repo_id = $1)")
            .bind(repo_id)
            .fetch_one(pool)
            .await?,
    )
}

pub async fn list_secrets(
    pool: &PgPool,
    kind: SecretKind,
    actor_id: i64,
    repo_id: i64,
) -> Result<Vec<RepoSecretMeta>, SecretsError> {
    let mut tx = pool.begin().await?;
    if !super::discussions::lock_settings_read_authority(&mut tx, actor_id, repo_id, true)
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
    >(&format!(
        "SELECT name, created_at, updated_at FROM {} WHERE repo_id = $1 ORDER BY name",
        kind.secrets_table(),
    ))
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
    kind: SecretKind,
    actor_id: impl Into<crate::backend::db::users::accounts::AccountActor>,
    repo_id: i64,
    name: &str,
) -> Result<bool, SecretsError> {
    let authority = actor_id.into();
    let actor_id = authority.user_id();
    let mut tx = pool.begin().await?;
    if !super::discussions::lock_admin_authority(&mut tx, actor_id, repo_id)
        .await
        .map_err(SecretsError::authority)?
    {
        return Err(SecretsError::Denied);
    }
    if let Some(credential) = authority.credential() {
        super::users::transitions::require_account(&mut tx, actor_id, &credential).await.map_err(SecretsError::authority)?;
    }
    let result = sqlx::query(&format!(
        "DELETE FROM {} WHERE repo_id = $1 AND name = $2",
        kind.secrets_table()
    ))
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
