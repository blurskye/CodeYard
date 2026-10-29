use crate::backend::db::secrets::{SecretKind, SecretsError};
use sqlx::SqlitePool;

use crate::backend::crypto::SecretEncryptor;
use crate::backend::db::models::{RepoSecret, RepoSecretMeta};

pub async fn create_or_update_secret(
    pool: &SqlitePool,
    encryptor: &SecretEncryptor,
    (repo_id, kind): (i64, SecretKind),
    name: &str,
    value: &str,
    created_by_id: Option<i64>,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
) -> Result<bool, SecretsError> {
    let actor_id = created_by_id.ok_or(SecretsError::Denied)?;
    let (encrypted_value, nonce) = encryptor.encrypt(value.as_bytes())?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !super::discussions::lock_admin_authority(&mut tx, actor_id, repo_id)
        .await
        .map_err(SecretsError::authority)?
    {
        return Err(SecretsError::Denied);
    }
    if !super::users::transitions::credential_live(&mut tx, actor_id, credential, true).await.map_err(SecretsError::authority)? { return Err(SecretsError::Denied); }
    let table = kind.secrets_table();
    let exists: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM {table} WHERE repo_id = ?1 AND name = ?2)",
    ))
    .bind(repo_id)
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    // A set larger than one CI claim can carry is refused here, so claims never load more.
    let (others, stored_bytes): (i64, i64) = sqlx::query_as(&format!(
        "SELECT COUNT(*), COALESCE(SUM(length(name) + length(encrypted_value)), 0) \
         FROM {table} WHERE repo_id = ?1 AND name <> ?2",
    ))
    .bind(repo_id)
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    crate::backend::db::secrets::ensure_room_for_secret(others, stored_bytes, name, value)?;

    sqlx::query(&format!(
        r#"
        INSERT INTO {table} (repo_id, name, encrypted_value, nonce, created_by_id)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT (repo_id, name) DO UPDATE SET
            encrypted_value = EXCLUDED.encrypted_value,
            nonce = EXCLUDED.nonce,
            updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
        "#,
    ))
    .bind(repo_id)
    .bind(name)
    .bind(&encrypted_value)
    .bind(&nonce[..])
    .bind(Some(actor_id))
    .execute(&mut *tx)
    .await?;
    if matches!(kind, SecretKind::Actions) { delivery_budget(&mut tx, repo_id).await?; }
    tx.commit().await?;

    Ok(!exists)
}

pub async fn repository_secret_key(
    pool: &SqlitePool,
    kind: SecretKind,
    encryptor: &SecretEncryptor,
    actor_id: i64,
    repo_id: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
) -> Result<(String, crypto_box::SecretKey), SecretsError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !super::discussions::lock_admin_authority(&mut tx, actor_id, repo_id)
        .await
        .map_err(SecretsError::authority)?
    {
        return Err(SecretsError::Denied);
    }
    if !super::users::transitions::credential_live(&mut tx, actor_id, credential, false).await.map_err(SecretsError::authority)? { return Err(SecretsError::Denied); }
    let (encrypted, nonce) = encryptor.generate_sealed_box_key()?;
    let table = kind.keys_table();
    sqlx::query(&format!(
        "INSERT INTO {table} (repo_id, key_id, encrypted_key, nonce) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(repo_id) DO NOTHING",
    ))
    .bind(repo_id)
    .bind(uuid::Uuid::new_v4().simple().to_string())
    .bind(encrypted)
    .bind(nonce.as_slice())
    .execute(&mut *tx)
    .await?;
    let (id, encrypted, nonce): (String, Vec<u8>, Vec<u8>) = sqlx::query_as(&format!(
        "SELECT key_id, encrypted_key, nonce FROM {table} WHERE repo_id = ?1",
    ))
    .bind(repo_id)
    .fetch_one(&mut *tx)
    .await?;
    let key = encryptor.sealed_box_key(&encrypted, &nonce)?;
    tx.commit().await?;
    Ok((id, key))
}

pub(crate) async fn delivery_budget(c: &mut sqlx::SqliteConnection, repo: i64) -> Result<(), SecretsError> {
    let (count, bytes): (i64, i64) = sqlx::query_as("WITH inherited AS (SELECT v.name,length(CAST(v.name AS BLOB))+length(v.encrypted_value)-16 AS bytes FROM org_actions_values v JOIN repositories r ON r.owner_kind='org' AND r.owner_org_id=v.org_id WHERE r.id=?1 AND v.kind='secret' AND (v.visibility='all' OR (v.visibility='private' AND r.is_private) OR (v.visibility='selected' AND EXISTS(SELECT 1 FROM org_actions_value_repositories s WHERE s.org_id=v.org_id AND s.kind=v.kind AND s.name=v.name AND s.repo_id=r.id))) ORDER BY v.name LIMIT 100), delivered AS (SELECT i.bytes FROM inherited i WHERE NOT EXISTS(SELECT 1 FROM repo_secrets s WHERE s.repo_id=?1 AND upper(s.name)=upper(i.name)) UNION ALL SELECT length(CAST(name AS BLOB))+length(encrypted_value)-16 FROM repo_secrets WHERE repo_id=?1) SELECT COUNT(*),COALESCE(SUM(bytes),0) FROM delivered")
        .bind(repo).fetch_one(c).await?;
    crate::backend::db::secrets::ensure_workflow_budget(count, bytes)
}

pub async fn repository_has_secrets(pool: &SqlitePool, repo_id: i64) -> Result<bool, SecretsError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repo_secrets WHERE repo_id = ?1)")
            .bind(repo_id)
            .fetch_one(pool)
            .await?,
    )
}

pub async fn list_secrets(
    pool: &SqlitePool,
    kind: SecretKind,
    actor_id: i64,
    repo_id: i64,
) -> Result<Vec<RepoSecretMeta>, SecretsError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
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
        "SELECT name, created_at, updated_at FROM {} WHERE repo_id = ?1 ORDER BY name",
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
    pool: &SqlitePool,
    kind: SecretKind,
    actor_id: i64,
    repo_id: i64,
    name: &str,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
) -> Result<bool, SecretsError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !super::discussions::lock_admin_authority(&mut tx, actor_id, repo_id)
        .await
        .map_err(SecretsError::authority)?
    {
        return Err(SecretsError::Denied);
    }
    if !super::users::transitions::credential_live(&mut tx, actor_id, credential, true).await.map_err(SecretsError::authority)? { return Err(SecretsError::Denied); }
    let result = sqlx::query(&format!(
        "DELETE FROM {} WHERE repo_id = ?1 AND name = ?2",
        kind.secrets_table()
    ))
    .bind(repo_id)
    .bind(name)
    .execute(&mut *tx)
    .await?;
    if matches!(kind, SecretKind::Actions) { delivery_budget(&mut tx, repo_id).await?; }
    tx.commit().await?;

    Ok(result.rows_affected() > 0)
}

pub async fn decrypt_repository_secrets(
    pool: &SqlitePool,
    encryptor: &SecretEncryptor,
    repo_id: i64,
) -> Result<Vec<(String, String)>, SecretsError> {
    let secrets = sqlx::query_as::<_, RepoSecret>("SELECT * FROM repo_secrets WHERE repo_id = ?1")
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
