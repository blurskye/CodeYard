//! Durable failed-login budgets per client address and per account under SQLite writer ownership.
use crate::backend::db::sqlite::time;
use crate::backend::db::users::admission::{Receipt, ACCOUNT_LIMIT, CLIENT_LIMIT, WINDOW_SECONDS};
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
) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    sqlx::query("INSERT INTO auth_login_limits(account_key,attempts,window_ends_at) VALUES (?1,0,?2) ON CONFLICT DO NOTHING")
        .bind(key).bind(now).execute(&mut *tx).await?;
    let window: Option<String> = sqlx::query_scalar("UPDATE auth_login_limits SET attempts=CASE WHEN window_ends_at<=?2 THEN 1 ELSE attempts+1 END,window_ends_at=CASE WHEN window_ends_at<=?2 THEN ?3 ELSE window_ends_at END WHERE account_key=?1 AND (window_ends_at<=?2 OR attempts<?4) RETURNING window_ends_at")
        .bind(key).bind(now).bind(window_ends_at).bind(limit)
        .fetch_optional(&mut *tx).await?;
    window.as_deref().map(time::decode).transpose()
}

/// Counts an attempt against the client address, when there is one, and the account. The client
/// budget is the outer limit: a client over it allocates no account row.
pub async fn admit_password_attempt(
    pool: &SqlitePool,
    client_key: Option<&str>,
    account_key: &str,
) -> Result<Option<Receipt>> {
    check_keys(client_key, account_key)?;
    // BEGIN IMMEDIATE plus bounded busy_timeout fails closed across connections
    // and processes. No row-lock imitation or per-process attempt authority.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let now = time::now();
    let encoded = time::encode(now);
    let window_ends_at = time::encode(now + Duration::seconds(WINDOW_SECONDS));
    let client_window = if let Some(key) = client_key {
        charge(&mut tx, key, &encoded, &window_ends_at, CLIENT_LIMIT).await?
    } else { None };
    let account_window = if client_key.is_none() || client_window.is_some() {
        charge(&mut tx, account_key, &encoded, &window_ends_at, ACCOUNT_LIMIT).await?
    } else { None };
    let receipt = account_window.map(|window| Receipt {
        client: client_key.zip(client_window).map(|(key, window)| (key.to_owned(), window)),
        account: (account_key.to_owned(), window),
    });
    if receipt.is_some() {
        sqlx::query("DELETE FROM auth_login_limits WHERE account_key IN (SELECT account_key FROM auth_login_limits WHERE window_ends_at<=?1 ORDER BY window_ends_at LIMIT 32)")
            .bind(&encoded).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(receipt)
}

/// Gives back the attempt that `admit_password_attempt` counted, after the credentials were right.
pub async fn forgive_password_attempt(
    pool: &SqlitePool,
    receipt: &Receipt,
) -> Result<()> {
    for (key, window) in receipt.client.iter().chain(std::iter::once(&receipt.account)) {
        sqlx::query("UPDATE auth_login_limits SET attempts=attempts-1 WHERE account_key=?1 AND attempts>0 AND window_ends_at=?2 AND window_ends_at>?3")
            .bind(key).bind(time::encode(*window)).bind(time::encode(time::now()))
            .execute(pool).await?;
    }
    Ok(())
}
