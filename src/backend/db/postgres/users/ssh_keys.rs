use crate::backend::db::models::{SshKey, User, UserWithKey};
use crate::backend::db::users::ssh_keys::{
    calculate_fingerprint, valid_stored_key, SshKeyWriteRefusal,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::debug;

/// Operator enrollment does not depend on a browser session.
pub async fn add_ssh_key(
    pool: &PgPool,
    user_id: i64,
    name: &str,
    public_key: &str,
) -> Result<SshKey> {
    insert_ssh_key(pool, user_id, None, name, public_key).await
}

pub async fn add_ssh_key_for_session(
    pool: &PgPool,
    user_id: i64,
    session_id: &str,
    name: &str,
    public_key: &str,
) -> Result<SshKey> {
    insert_ssh_key(pool, user_id, Some(session_id), name, public_key).await
}

async fn insert_ssh_key(
    pool: &PgPool,
    user_id: i64,
    session_id: Option<&str>,
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

    let mut tx = pool.begin().await?;
    let active: Option<bool> = sqlx::query_scalar(
        "SELECT NOT is_banned AND NOT must_change_password FROM users WHERE id = $1 FOR SHARE",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    anyhow::ensure!(active == Some(true), SshKeyWriteRefusal::AccountRestricted);
    // Serialize enrollments of this fingerprint so that only one can pass the check below.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 37000003))")
        .bind(&fingerprint)
        .execute(&mut *tx)
        .await?;
    let used: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ssh_keys WHERE fingerprint = $1)")
            .bind(&fingerprint)
            .fetch_one(&mut *tx)
            .await?;
    anyhow::ensure!(!used, SshKeyWriteRefusal::AlreadyUsed);
    if let Some(session_id) = session_id {
        // A separate statement sees revocation committed while the account lock waited.
        let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=$1 AND user_id=$2 AND expires_at>clock_timestamp())")
            .bind(session_id).bind(user_id).fetch_one(&mut *tx).await?;
        anyhow::ensure!(live, SshKeyWriteRefusal::InvalidSession);
    }
    let result = sqlx::query_as::<_, SshKey>(
        r#"
        INSERT INTO ssh_keys (user_id, name, fingerprint, public_key, key_type)
        VALUES ($1, $2, $3, $4, $5)
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
    // The unique fingerprint index is the final guard; report its violation as the refusal above.
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
    pool: &PgPool,
    fingerprint: &str,
) -> Result<Option<UserWithKey>> {
    debug!("Looking up user by key fingerprint: {}", fingerprint);

    #[derive(sqlx::FromRow)]
    struct JoinedRow {
        u_id: i64,
        u_username: String,
        u_email: Option<String>,
        u_password_hash: Option<String>,
        u_avatar_file_id: Option<uuid::Uuid>,
        u_display_name: Option<String>,
        u_bio: Option<String>,
        u_location: Option<String>,
        u_website: Option<String>,
        u_is_admin: bool,
        u_is_banned: bool,
        u_must_change_password: bool,
        u_email_verified: bool,
        u_two_factor_enabled: bool,
        u_created_at: DateTime<Utc>,
        u_updated_at: DateTime<Utc>,
        k_id: i64,
        k_user_id: i64,
        k_name: String,
        k_fingerprint: String,
        k_public_key: String,
        k_key_type: String,
        k_last_used_at: Option<DateTime<Utc>>,
        k_created_at: DateTime<Utc>,
    }

    let result = sqlx::query_as::<_, JoinedRow>(
        r#"
        SELECT
            u.id as u_id, u.username as u_username, u.email as u_email,
            u.password_hash as u_password_hash, u.avatar_file_id as u_avatar_file_id,
            u.display_name as u_display_name, u.bio as u_bio, u.location as u_location,
            u.website as u_website, u.is_admin as u_is_admin,
            u.is_banned as u_is_banned,
            u.must_change_password as u_must_change_password,
            u.email_verified as u_email_verified,
            u.two_factor_enabled as u_two_factor_enabled,
            u.created_at as u_created_at, u.updated_at as u_updated_at,
            k.id as k_id, k.user_id as k_user_id, k.name as k_name,
            k.fingerprint as k_fingerprint, k.public_key as k_public_key,
            k.key_type as k_key_type, k.last_used_at as k_last_used_at, k.created_at as k_created_at
        FROM users u
        INNER JOIN ssh_keys k ON u.id = k.user_id
        WHERE k.fingerprint = $1
        "#,
    )
    .bind(fingerprint)
    .fetch_optional(pool)
    .await?;

    Ok(result
        .map(|row| UserWithKey {
            user: User {
                id: row.u_id,
                username: row.u_username,
                email: row.u_email,
                password_hash: row.u_password_hash,
                avatar_file_id: row.u_avatar_file_id,
                display_name: row.u_display_name,
                bio: row.u_bio,
                location: row.u_location,
                website: row.u_website,
                is_admin: row.u_is_admin,
                is_banned: row.u_is_banned,
                must_change_password: row.u_must_change_password,
                email_verified: row.u_email_verified,
                two_factor_enabled: row.u_two_factor_enabled,
                created_at: row.u_created_at,
                updated_at: row.u_updated_at,
            },
            key: SshKey {
                id: row.k_id,
                user_id: row.k_user_id,
                name: row.k_name,
                fingerprint: row.k_fingerprint,
                public_key: row.k_public_key,
                key_type: row.k_key_type,
                last_used_at: row.k_last_used_at,
                created_at: row.k_created_at,
            },
        })
        .filter(|identity| valid_stored_key(&identity.key)))
}

/// Resolve an SSH helper's key ID against live rows; deleted keys cannot authenticate.
pub async fn get_user_by_ssh_key_id(pool: &PgPool, key_id: i64) -> Result<Option<UserWithKey>> {
    let fingerprint =
        sqlx::query_scalar::<_, String>("SELECT fingerprint FROM ssh_keys WHERE id = $1")
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

pub async fn list_user_keys(pool: &PgPool, user_id: i64) -> Result<Vec<SshKey>> {
    let keys = sqlx::query_as::<_, SshKey>(
        "SELECT * FROM ssh_keys WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(keys)
}

pub async fn delete_user_ssh_key(pool: &PgPool, user_id: i64, key_id: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let active: Option<bool> = sqlx::query_scalar(
        "SELECT NOT is_banned AND NOT must_change_password FROM users WHERE id = $1 FOR SHARE",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    anyhow::ensure!(active == Some(true), SshKeyWriteRefusal::AccountRestricted);
    let result = sqlx::query("DELETE FROM ssh_keys WHERE id = $1 AND user_id = $2")
        .bind(key_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(result.rows_affected() > 0)
}

pub async fn delete_ssh_key(pool: &PgPool, key_id: i64) -> Result<bool> {
    let result = sqlx::query("DELETE FROM ssh_keys WHERE id = $1")
        .bind(key_id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}
