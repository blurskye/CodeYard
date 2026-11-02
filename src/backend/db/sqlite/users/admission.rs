//! Durable failed-login budgets per client address and per account under SQLite writer ownership.
use crate::backend::db::sqlite::time;
use crate::backend::db::users::admission::{ACCOUNT_LIMIT, CLIENT_LIMIT, WINDOW_SECONDS};
use anyhow::Result;
use chrono::Duration;
use sqlx::{SqliteConnection, SqlitePool};

fn check_keys(client_key: Option<&str>, account_key: &str) -> Result<()> {
    let valid = |key: &str| !key.is_empty() && key.len() <= 80;
    if !valid(account_key) || !client_key.is_none_or(valid) {
        anyhow::bail!("Invalid authentication admission key");
    }
    Ok(())
}

/// Counts one attempt against `key` unless its budget for the current window is spent.
async fn charge(
    tx: &mut SqliteConnection,
    key: &str,
    now: &str,
    window_ends_at: &str,
    limit: i32,
) -> Result<bool> {
    sqlx::query("INSERT INTO auth_login_limits(account_key,attempts,window_ends_at) VALUES (?1,0,?2) ON CONFLICT DO NOTHING")
        .bind(key).bind(now).execute(&mut *tx).await?;
    let counted = sqlx::query("UPDATE auth_login_limits SET attempts=CASE WHEN window_ends_at<=?2 THEN 1 ELSE attempts+1 END,window_ends_at=CASE WHEN window_ends_at<=?2 THEN ?3 ELSE window_ends_at END WHERE account_key=?1 AND (window_ends_at<=?2 OR attempts<?4)")
        .bind(key).bind(now).bind(window_ends_at).bind(limit)
        .execute(&mut *tx).await?.rows_affected() == 1;
    Ok(counted)
}

/// Counts an attempt against the client address, when there is one, and the account. The client
/// budget is the outer limit: a client over it allocates no account row.
pub async fn admit_password_attempt(
    pool: &SqlitePool,
    client_key: Option<&str>,
    account_key: &str,
) -> Result<bool> {
    check_keys(client_key, account_key)?;
    // BEGIN IMMEDIATE plus bounded busy_timeout fails closed across connections
    // and processes. No row-lock imitation or per-process attempt authority.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let now = time::now();
    let encoded = time::encode(now);
    let window_ends_at = time::encode(now + Duration::seconds(WINDOW_SECONDS));
    let mut admitted = true;
    if let Some(key) = client_key {
        admitted = charge(&mut tx, key, &encoded, &window_ends_at, CLIENT_LIMIT).await?;
    }
    if admitted {
        admitted = charge(
            &mut tx,
            account_key,
            &encoded,
            &window_ends_at,
            ACCOUNT_LIMIT,
        )
        .await?;
    }
    if admitted {
        sqlx::query("DELETE FROM auth_login_limits WHERE account_key IN (SELECT account_key FROM auth_login_limits WHERE window_ends_at<=?1 ORDER BY window_ends_at LIMIT 32)")
            .bind(&encoded).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(admitted)
}

/// Gives back the attempt that `admit_password_attempt` counted, after the credentials were right.
pub async fn forgive_password_attempt(
    pool: &SqlitePool,
    client_key: Option<&str>,
    account_key: &str,
) -> Result<()> {
    check_keys(client_key, account_key)?;
    sqlx::query("UPDATE auth_login_limits SET attempts=attempts-1 WHERE account_key IN (?1,?2) AND attempts>0 AND window_ends_at>?3")
        .bind(account_key).bind(client_key.unwrap_or(account_key)).bind(time::encode(time::now()))
        .execute(pool).await?;
    Ok(())
}
