use crate::backend::db::users::accounts::AccountCredential;
use crate::backend::db::users::gpg_keys::GpgKey;
use crate::backend::db::users::gpg_keys::GpgKeyRefusal;
use crate::backend::db::users::gpg_keys::GpgKeySummary;
use crate::backend::db::users::gpg_keys::NewGpgKey;
use crate::backend::db::users::gpg_keys::StoredGpgKey;
use crate::backend::db::users::gpg_keys::StoredGpgKeySummary;
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};

pub async fn list_gpg_keys(
    pool: &SqlitePool,
    user: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<GpgKey>> {
    sqlx::query_as::<_,StoredGpgKey>("SELECT * FROM gpg_keys WHERE user_id=?1 AND primary_key_id IS NULL ORDER BY id LIMIT ?2 OFFSET ?3").bind(user).bind(limit).bind(offset).fetch_all(pool).await?.into_iter().map(StoredGpgKey::decode).collect()
}
pub async fn list_gpg_key_summaries(
    pool: &SqlitePool,
    user: i64,
    credential: &AccountCredential<'_>,
) -> Result<Vec<GpgKeySummary>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::backend::db::sqlite::users::transitions::require_account(&mut tx, user, credential)
        .await?;
    let keys = sqlx::query_as::<_, StoredGpgKeySummary>("SELECT id,name,fingerprint,key_id,json_extract(key_data,'$.created_at') AS created_at,json_extract(key_data,'$.expires_at') AS expires_at,json_extract(key_data,'$.revoked') AS revoked FROM gpg_keys WHERE user_id=?1 AND primary_key_id IS NULL ORDER BY id LIMIT 100")
        .bind(user).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    keys.into_iter().map(StoredGpgKeySummary::decode).collect()
}
pub async fn get_gpg_key(pool: &SqlitePool, user: i64, id: i64) -> Result<Option<GpgKey>> {
    sqlx::query_as::<_, StoredGpgKey>(
        "SELECT * FROM gpg_keys WHERE user_id=?1 AND id=?2 AND primary_key_id IS NULL",
    )
    .bind(user)
    .bind(id)
    .fetch_optional(pool)
    .await?
    .map(StoredGpgKey::decode)
    .transpose()
}
/// Keys, primary or subkey, that a signature's issuer fingerprint or 16-digit key id names.
/// Key ids are not unique, so a few may match.
pub async fn find_signing_keys(pool: &SqlitePool, issuer: &str) -> Result<Vec<GpgKey>> {
    sqlx::query_as::<_, StoredGpgKey>(
        "SELECT * FROM gpg_keys WHERE fingerprint=?1 OR key_id=?1 ORDER BY id LIMIT 8",
    )
    .bind(issuer)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(StoredGpgKey::decode)
    .collect()
}
pub async fn list_subkeys(pool: &SqlitePool, user: i64, parents: &[i64]) -> Result<Vec<GpgKey>> {
    anyhow::ensure!(parents.len() <= 101, "Key page exceeds its limit");
    sqlx::query_as::<_,StoredGpgKey>("SELECT * FROM gpg_keys WHERE user_id=?1 AND primary_key_id IN (SELECT value FROM json_each(?2)) ORDER BY id").bind(user).bind(serde_json::to_string(parents)?).fetch_all(pool).await?.into_iter().map(StoredGpgKey::decode).collect()
}
async fn insert(
    tx: &mut Transaction<'_, Sqlite>,
    user: i64,
    parent: Option<i64>,
    name: Option<&str>,
    key: &NewGpgKey,
) -> Result<GpgKey> {
    let result = sqlx::query_as::<_,StoredGpgKey>("INSERT INTO gpg_keys(user_id,primary_key_id,name,fingerprint,key_id,key_data) VALUES(?1,?2,?3,?4,?5,?6) RETURNING *")
        .bind(user).bind(parent).bind(name).bind(&key.fingerprint).bind(&key.key_id).bind(serde_json::to_string(&key.material)?).fetch_one(&mut **tx).await;
    match result {
        Ok(key) => key.decode(),
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            Err(GpgKeyRefusal::AlreadyUsed.into())
        }
        Err(error) => Err(error.into()),
    }
}
pub async fn add_gpg_key(
    pool: &SqlitePool,
    user: i64,
    credential: &AccountCredential<'_>,
    name: Option<&str>,
    key: &NewGpgKey,
) -> Result<GpgKey> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::backend::db::sqlite::users::transitions::require_account(&mut tx, user, credential)
        .await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM gpg_keys WHERE user_id=?1 AND primary_key_id IS NULL",
    )
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(count < 100, GpgKeyRefusal::Limit);
    let root = insert(&mut tx, user, None, name, key).await?;
    for subkey in &key.subkeys {
        insert(&mut tx, user, Some(root.id), None, subkey).await?;
    }
    tx.commit().await?;
    Ok(root)
}
pub async fn delete_gpg_key(
    pool: &SqlitePool,
    user: i64,
    credential: &AccountCredential<'_>,
    id: i64,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::backend::db::sqlite::users::transitions::require_account(&mut tx, user, credential)
        .await?;
    let changed =
        sqlx::query("DELETE FROM gpg_keys WHERE user_id=?1 AND id=?2 AND primary_key_id IS NULL")
            .bind(user)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
    tx.commit().await?;
    Ok(changed)
}
