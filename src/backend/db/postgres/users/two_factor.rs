use crate::backend::db::users::two_factor::StoredTwoFactor;
use anyhow::Result;
use sqlx::PgPool;

pub async fn get_two_factor(pool: &PgPool, user_id: i64) -> Result<Option<StoredTwoFactor>> {
    let row: Option<(Vec<u8>, Vec<u8>, Option<i64>)> = sqlx::query_as(
        "SELECT totp_secret_encrypted, totp_nonce, last_used_step FROM two_factor_auth WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(secret_encrypted, nonce, last_used_step)| StoredTwoFactor {
            secret_encrypted,
            nonce,
            last_used_step,
        },
    ))
}

pub async fn enable_two_factor(
    pool: &PgPool,
    user_id: i64,
    secret_encrypted: &[u8],
    nonce: &[u8],
    recovery_hashes: &[String],
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO two_factor_auth (user_id, totp_secret_encrypted, totp_nonce) VALUES ($1, $2, $3) ON CONFLICT (user_id) DO NOTHING",
    )
    .bind(user_id)
    .bind(secret_encrypted)
    .bind(nonce)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if !inserted {
        return Ok(false);
    }
    for hash in recovery_hashes {
        sqlx::query("INSERT INTO two_factor_recovery_codes (user_id, code_hash) VALUES ($1, $2)")
            .bind(user_id)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE users SET two_factor_enabled = TRUE WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn use_totp_step(pool: &PgPool, user_id: i64, step: i64) -> Result<bool> {
    let changed = sqlx::query(
        "UPDATE two_factor_auth SET last_used_step = $2 WHERE user_id = $1 AND (last_used_step IS NULL OR last_used_step < $2)",
    )
    .bind(user_id)
    .bind(step)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

pub async fn use_recovery_code(pool: &PgPool, user_id: i64, code_hash: &str) -> Result<bool> {
    let removed =
        sqlx::query("DELETE FROM two_factor_recovery_codes WHERE user_id = $1 AND code_hash = $2")
            .bind(user_id)
            .bind(code_hash)
            .execute(pool)
            .await?
            .rows_affected();
    Ok(removed == 1)
}

pub async fn replace_recovery_codes(
    pool: &PgPool,
    user_id: i64,
    recovery_hashes: &[String],
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    // Locks the enabled row, so a concurrent disable cannot leave codes behind.
    let enabled: Option<i64> =
        sqlx::query_scalar("SELECT user_id FROM two_factor_auth WHERE user_id = $1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    if enabled.is_none() {
        return Ok(false);
    }
    sqlx::query("DELETE FROM two_factor_recovery_codes WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for hash in recovery_hashes {
        sqlx::query("INSERT INTO two_factor_recovery_codes (user_id, code_hash) VALUES ($1, $2)")
            .bind(user_id)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn disable_two_factor(pool: &PgPool, user_id: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let removed = sqlx::query("DELETE FROM two_factor_auth WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    sqlx::query("DELETE FROM two_factor_recovery_codes WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE users SET two_factor_enabled = FALSE WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(removed)
}
