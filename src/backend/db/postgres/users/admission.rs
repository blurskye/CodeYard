// Shared, bounded PostgreSQL password-attempt admission. No per-node authority.
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// Canonical account key or fixed-size hash of an unknown identity, never plaintext credentials.
/// Global denial allocates no account row. Success never resets concurrent budgets.
pub async fn admit_password_attempt(pool: &PgPool, account_key: &str) -> Result<bool> {
    if account_key.is_empty() || account_key.len() > 80 || account_key == "global" {
        anyhow::bail!("Invalid authentication admission key");
    }
    let mut tx = pool.begin().await?;
    // An unavailable/busy shared store fails closed, without a process-local fallback.
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO auth_login_limits (account_key,attempts,window_ends_at) VALUES ('global',0,NOW()) ON CONFLICT (account_key) DO NOTHING")
        .execute(&mut *tx).await?;
    // This singleton row is the cross-node serialization/admission boundary.
    sqlx::query("SELECT account_key FROM auth_login_limits WHERE account_key='global' FOR UPDATE")
        .execute(&mut *tx)
        .await?;
    let now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let global = sqlx::query("UPDATE auth_login_limits SET attempts=CASE WHEN window_ends_at<=$1 THEN 1 ELSE attempts+1 END,window_ends_at=CASE WHEN window_ends_at<=$1 THEN $1+INTERVAL '60 seconds' ELSE window_ends_at END WHERE account_key='global' AND (window_ends_at<=$1 OR attempts<120)")
        .bind(now).execute(&mut *tx).await?.rows_affected() == 1;
    if !global {
        tx.commit().await?;
        return Ok(false);
    }
    // Only our new transient limiter rows are cleaned. Never sessions/accounts/repos.
    // One bounded indexed batch per admitted global attempt; no unbounded maintenance scan.
    sqlx::query("DELETE FROM auth_login_limits WHERE account_key IN (SELECT account_key FROM auth_login_limits WHERE account_key<>'global' AND window_ends_at<=$1 ORDER BY window_ends_at LIMIT 32 FOR UPDATE SKIP LOCKED)")
        .bind(now).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO auth_login_limits (account_key,attempts,window_ends_at) VALUES ($1,0,$2) ON CONFLICT (account_key) DO NOTHING")
        .bind(account_key).bind(now).execute(&mut *tx).await?;
    let account = sqlx::query("UPDATE auth_login_limits SET attempts=CASE WHEN window_ends_at<=$2 THEN 1 ELSE attempts+1 END,window_ends_at=CASE WHEN window_ends_at<=$2 THEN $2+INTERVAL '300 seconds' ELSE window_ends_at END WHERE account_key=$1 AND (window_ends_at<=$2 OR attempts<10)")
        .bind(account_key).bind(now).execute(&mut *tx).await?.rows_affected() == 1;
    // Failed account attempts still consume the global budget, limiting key allocation
    // to 120/minute and live account state to roughly five such windows.
    tx.commit().await?;
    Ok(account)
}
