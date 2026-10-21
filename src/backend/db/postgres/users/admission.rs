// Shared, bounded PostgreSQL failed-login budgets per client address and per account. No per-node
// authority.
use crate::backend::db::users::admission::{ACCOUNT_LIMIT, CLIENT_LIMIT, WINDOW_SECONDS};
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use sqlx::{PgConnection, PgPool};

fn check_keys(client_key: Option<&str>, account_key: &str) -> Result<()> {
    let valid = |key: &str| !key.is_empty() && key.len() <= 80;
    if !valid(account_key) || !client_key.is_none_or(valid) {
        anyhow::bail!("Invalid authentication admission key");
    }
    Ok(())
}

/// Counts one attempt against `key` unless its budget for the current window is spent. The
/// single UPDATE locks the row, so concurrent attempts on one key are counted one by one.
async fn charge(
    tx: &mut PgConnection,
    key: &str,
    now: DateTime<Utc>,
    window_ends_at: DateTime<Utc>,
    limit: i32,
) -> Result<bool> {
    sqlx::query("INSERT INTO auth_login_limits (account_key,attempts,window_ends_at) VALUES ($1,0,$2) ON CONFLICT (account_key) DO NOTHING")
        .bind(key).bind(now).execute(&mut *tx).await?;
    let counted = sqlx::query("UPDATE auth_login_limits SET attempts=CASE WHEN window_ends_at<=$2 THEN 1 ELSE attempts+1 END,window_ends_at=CASE WHEN window_ends_at<=$2 THEN $3 ELSE window_ends_at END WHERE account_key=$1 AND (window_ends_at<=$2 OR attempts<$4)")
        .bind(key).bind(now).bind(window_ends_at).bind(limit)
        .execute(&mut *tx).await?.rows_affected() == 1;
    Ok(counted)
}

/// Counts an attempt against the client address, when there is one, and the account. The client
/// budget is the outer limit: a client over it allocates no account row. Client rows are always
/// locked before account rows, so concurrent attempts cannot deadlock.
pub async fn admit_password_attempt(
    pool: &PgPool,
    client_key: Option<&str>,
    account_key: &str,
) -> Result<bool> {
    check_keys(client_key, account_key)?;
    let mut tx = pool.begin().await?;
    // An unavailable/busy shared store fails closed, without a process-local fallback.
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await?;
    let now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let window_ends_at = now + Duration::seconds(WINDOW_SECONDS);
    let mut admitted = true;
    if let Some(key) = client_key {
        admitted = charge(&mut tx, key, now, window_ends_at, CLIENT_LIMIT).await?;
    }
    if admitted {
        admitted = charge(&mut tx, account_key, now, window_ends_at, ACCOUNT_LIMIT).await?;
    }
    if admitted {
        // Only our transient limiter rows are cleaned, one bounded indexed batch per admitted attempt.
        sqlx::query("DELETE FROM auth_login_limits WHERE account_key IN (SELECT account_key FROM auth_login_limits WHERE window_ends_at<=$1 ORDER BY window_ends_at LIMIT 32 FOR UPDATE SKIP LOCKED)")
            .bind(now).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(admitted)
}

/// Gives back the attempt that `admit_password_attempt` counted, after the credentials were right.
/// One statement per key, so no statement holds two limiter rows.
pub async fn forgive_password_attempt(
    pool: &PgPool,
    client_key: Option<&str>,
    account_key: &str,
) -> Result<()> {
    check_keys(client_key, account_key)?;
    for key in client_key.into_iter().chain([account_key]) {
        sqlx::query("UPDATE auth_login_limits SET attempts=attempts-1 WHERE account_key=$1 AND attempts>0 AND window_ends_at>NOW()")
            .bind(key).execute(pool).await?;
    }
    Ok(())
}
