use crate::backend::db::users::accounts::AccountCredential;
use crate::backend::db::users::ssh_keys::calculate_fingerprint;
use crate::backend::db::users::ssh_keys::valid_stored_key;
use anyhow::Result;
use codeyard_db_core::models::SshKey;
use codeyard_db_core::models::User;
use codeyard_db_core::models::UserWithKey;
use codeyard_model::identity::SshKeyWriteRefusal;
use codeyard_support::credentials::AccountRestricted;
use sqlx::SqlitePool;
use tracing::debug;

pub async fn touch_ssh_key(pool: &SqlitePool, key_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE ssh_keys SET last_used_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1",
    )
    .bind(key_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Operator enrollment does not depend on a browser session.
pub async fn add_ssh_key(
    pool: &SqlitePool,
    user_id: i64,
    name: &str,
    public_key: &str,
) -> Result<SshKey> {
    insert_ssh_key(pool, user_id, None, name, public_key).await
}

pub async fn add_ssh_key_for_session(
    pool: &SqlitePool,
    user_id: i64,
    session_id: &str,
    name: &str,
    public_key: &str,
) -> Result<SshKey> {
    insert_ssh_key(
        pool,
        user_id,
        Some(&AccountCredential::Session(session_id)),
        name,
        public_key,
    )
    .await
}

pub async fn add_ssh_key_for_token(
    pool: &SqlitePool,
    user_id: i64,
    token: i64,
    hash: &str,
    name: &str,
    public_key: &str,
) -> Result<SshKey> {
    insert_ssh_key(
        pool,
        user_id,
        Some(&AccountCredential::Token(token, hash)),
        name,
        public_key,
    )
    .await
}

async fn require_key_account(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user: i64,
    credential: Option<&AccountCredential<'_>>,
) -> Result<()> {
    if let Some(credential @ AccountCredential::Token(_, _)) = credential {
        return super::transitions::require_account(tx, user, credential)
            .await
            .map_err(|error| {
                if error.is::<AccountRestricted>() {
                    anyhow::Error::from(SshKeyWriteRefusal::AccountRestricted)
                } else {
                    error
                }
            });
    }
    let active: Option<bool> = sqlx::query_scalar(
        "SELECT NOT is_banned AND NOT must_change_password FROM users WHERE id=?1",
    )
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?;
    anyhow::ensure!(active == Some(true), SshKeyWriteRefusal::AccountRestricted);
    if let Some(AccountCredential::Session(session)) = credential {
        anyhow::ensure!(
            super::transitions::live_session(tx, user, session).await?,
            SshKeyWriteRefusal::InvalidSession
        );
    }
    Ok(())
}

async fn insert_ssh_key(
    pool: &SqlitePool,
    user_id: i64,
    credential: Option<&AccountCredential<'_>>,
    name: &str,
    public_key: &str,
) -> Result<SshKey> {
    let public_key = public_key.trim();
    let fingerprint = calculate_fingerprint(public_key)?;

    let key_type = public_key
        .split_whitespace()
        .next()
        .unwrap_or("unknown")
        .to_string();

    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    require_key_account(&mut tx, user_id, credential).await?;
    let used: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ssh_keys WHERE fingerprint = ?1) OR EXISTS(SELECT 1 FROM deploy_keys WHERE fingerprint = ?1)")
            .bind(&fingerprint)
            .fetch_one(&mut *tx)
            .await?;
    anyhow::ensure!(!used, SshKeyWriteRefusal::AlreadyUsed);
    let result = sqlx::query_as::<_, SshKey>(
        r#"
        INSERT INTO ssh_keys (user_id, name, fingerprint, public_key, key_type)
        VALUES (?1, ?2, ?3, ?4, ?5)
        RETURNING *
        "#,
    )
    .bind(user_id)
    .bind(name)
    .bind(&fingerprint)
    .bind(public_key)
    .bind(&key_type)
    .fetch_one(&mut *tx)
    .await
    // The unique index is the final guard; report its violation as the refusal above.
    .map_err(|error| match &error {
        sqlx::Error::Database(database) if database.is_unique_violation() => {
            anyhow::Error::from(SshKeyWriteRefusal::AlreadyUsed)
        }
        _ => anyhow::Error::from(error),
    })?;
    tx.commit().await?;

    Ok(result)
}

pub async fn get_user_by_key_fingerprint(
    pool: &SqlitePool,
    fingerprint: &str,
) -> Result<Option<UserWithKey>> {
    debug!("Looking up user by key fingerprint: {}", fingerprint);

    let Some(key) = sqlx::query_as::<_, SshKey>("SELECT * FROM ssh_keys WHERE fingerprint = ?1")
        .bind(fingerprint)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?1")
        .bind(key.user_id)
        .fetch_optional(pool)
        .await?;

    Ok(user
        .map(|user| UserWithKey { user, key })
        .filter(|identity| valid_stored_key(&identity.key)))
}

/// Resolve an SSH helper's key ID against live rows; deleted keys cannot authenticate. A
/// fingerprint that is also a deploy key is ambiguous, as it is at authentication, so it
/// resolves to no account.
pub async fn get_user_by_ssh_key_id(pool: &SqlitePool, key_id: i64) -> Result<Option<UserWithKey>> {
    let fingerprint =
        sqlx::query_scalar::<_, String>("SELECT fingerprint FROM ssh_keys WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM deploy_keys d WHERE d.fingerprint = ssh_keys.fingerprint)")
            .bind(key_id)
            .fetch_optional(pool)
            .await?;
    let Some(fingerprint) = fingerprint else {
        return Ok(None);
    };
    // Use the joined live-key/user read last, so a deletion between queries
    // cannot return a previously fetched key attached to a still-live account.
    Ok(get_user_by_key_fingerprint(pool, &fingerprint)
        .await?
        .filter(|identity| identity.key.id == key_id))
}

pub async fn list_user_key_summaries(
    pool: &SqlitePool,
    user_id: i64,
    credential: &AccountCredential<'_>,
    after: i64,
) -> Result<Vec<crate::backend::db::users::ssh_keys::SshKeySummary>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::transitions::require_account(&mut tx, user_id, credential).await?;
    let keys = sqlx::query_as("SELECT id,name,fingerprint,key_type,created_at FROM ssh_keys WHERE user_id=?1 AND id>?2 ORDER BY id LIMIT 100")
        .bind(user_id).bind(after.max(0)).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(keys)
}

pub async fn user_keys_page(
    pool: &SqlitePool,
    user_id: i64,
    offset: i64,
    limit: i64,
) -> Result<Vec<SshKey>> {
    Ok(sqlx::query_as::<_, SshKey>(
        "SELECT * FROM ssh_keys WHERE user_id = ?1 ORDER BY created_at, id LIMIT ?2 OFFSET ?3",
    )
    .bind(user_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

pub async fn get_user_key(pool: &SqlitePool, user_id: i64, key_id: i64) -> Result<Option<SshKey>> {
    Ok(
        sqlx::query_as::<_, SshKey>("SELECT * FROM ssh_keys WHERE user_id = ?1 AND id = ?2")
            .bind(user_id)
            .bind(key_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn list_user_keys(pool: &SqlitePool, user_id: i64) -> Result<Vec<SshKey>> {
    let keys = sqlx::query_as::<_, SshKey>(
        "SELECT * FROM ssh_keys WHERE user_id = ?1 ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(keys)
}

pub async fn delete_user_ssh_key(
    pool: &SqlitePool,
    user_id: i64,
    credential: &AccountCredential<'_>,
    key_id: i64,
) -> Result<bool> {
    delete_key(pool, user_id, Some(credential), key_id).await
}

pub async fn delete_user_ssh_key_for_token(
    pool: &SqlitePool,
    user_id: i64,
    token: i64,
    hash: &str,
    key_id: i64,
) -> Result<bool> {
    delete_key(
        pool,
        user_id,
        Some(&AccountCredential::Token(token, hash)),
        key_id,
    )
    .await
}

async fn delete_key(
    pool: &SqlitePool,
    user_id: i64,
    credential: Option<&AccountCredential<'_>>,
    key_id: i64,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    require_key_account(&mut tx, user_id, credential).await?;
    let result = sqlx::query("DELETE FROM ssh_keys WHERE id = ?1 AND user_id = ?2")
        .bind(key_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(result.rows_affected() > 0)
}

pub async fn delete_ssh_key(pool: &SqlitePool, key_id: i64) -> Result<bool> {
    let result = sqlx::query("DELETE FROM ssh_keys WHERE id = ?1")
        .bind(key_id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}
