use crate::backend::db::users::{
    accounts::AccountCredential,
    signing_keys::{SigningKey, SigningKeyOwner},
    ssh_keys::{parse_public_key, SshKeyWriteRefusal},
};
use anyhow::Result;
use sqlx::PgPool;

pub async fn list_signing_key_summaries(
    pool: &PgPool,
    user: i64,
    credential: &AccountCredential<'_>,
    after: i64,
) -> Result<Vec<crate::backend::db::users::ssh_keys::SshKeySummary>> {
    let mut tx = pool.begin().await?;
    super::transitions::require_account(&mut tx, user, credential).await?;
    let keys = sqlx::query_as("SELECT id,title AS name,fingerprint,split_part(key,' ',1) AS key_type,created_at FROM ssh_signing_keys WHERE user_id=$1 AND id>$2 ORDER BY id LIMIT 100")
        .bind(user).bind(after.max(0)).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(keys)
}

pub async fn find_signing_key(pool: &PgPool, fingerprint: &str) -> Result<Option<SigningKeyOwner>> {
    Ok(
        sqlx::query_as("SELECT user_id,key FROM ssh_signing_keys WHERE fingerprint=$1")
            .bind(fingerprint)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn list_signing_keys(
    pool: &PgPool,
    user: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<SigningKey>> {
    Ok(sqlx::query_as("SELECT id,title,key,created_at FROM ssh_signing_keys WHERE user_id=$1 ORDER BY id LIMIT $2 OFFSET $3").bind(user).bind(limit).bind(offset).fetch_all(pool).await?)
}
pub async fn get_signing_key(pool: &PgPool, user: i64, id: i64) -> Result<Option<SigningKey>> {
    Ok(sqlx::query_as(
        "SELECT id,title,key,created_at FROM ssh_signing_keys WHERE user_id=$1 AND id=$2",
    )
    .bind(user)
    .bind(id)
    .fetch_optional(pool)
    .await?)
}
pub async fn add_signing_key(
    pool: &PgPool,
    user: i64,
    credential: &AccountCredential<'_>,
    title: &str,
    key: &str,
) -> Result<SigningKey> {
    let key = parse_public_key(key)?;
    let fingerprint = key.fingerprint(ssh_key::HashAlg::Sha256).to_string();
    let key = key.to_openssh()?;
    let mut tx = pool.begin().await?;
    super::transitions::require_account(&mut tx, user, credential).await?;
    let row = sqlx::query_as("INSERT INTO ssh_signing_keys(user_id,title,key,fingerprint) VALUES($1,$2,$3,$4) RETURNING id,title,key,created_at")
        .bind(user).bind(title).bind(key).bind(fingerprint).fetch_one(&mut *tx).await.map_err(|error|match &error {
            sqlx::Error::Database(error) if error.is_unique_violation() => anyhow::Error::from(SshKeyWriteRefusal::AlreadyUsed),
            _ => anyhow::Error::from(error),
        })?;
    tx.commit().await?;
    Ok(row)
}
pub async fn delete_signing_key(
    pool: &PgPool,
    user: i64,
    credential: &AccountCredential<'_>,
    id: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    super::transitions::require_account(&mut tx, user, credential).await?;
    let deleted = sqlx::query("DELETE FROM ssh_signing_keys WHERE user_id=$1 AND id=$2")
        .bind(user)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        > 0;
    tx.commit().await?;
    Ok(deleted)
}
