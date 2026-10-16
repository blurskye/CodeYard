//! Durable account/global attempt budgets under SQLite writer ownership.
use crate::backend::db::sqlite::time;
use anyhow::Result;
use chrono::Duration;
use sqlx::SqlitePool;

pub async fn admit_password_attempt(pool: &SqlitePool, account_key: &str) -> Result<bool> {
    if account_key.is_empty() || account_key.len() > 80 || account_key == "global" {
        anyhow::bail!("Invalid authentication admission key");
    }
    // BEGIN IMMEDIATE plus bounded busy_timeout fails closed across connections
    // and processes. No row-lock imitation or per-process attempt authority.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let now = time::now();
    let encoded = time::encode(now);
    sqlx::query("INSERT INTO auth_login_limits(account_key,attempts,window_ends_at) VALUES ('global',0,?1) ON CONFLICT DO NOTHING")
        .bind(&encoded).execute(&mut *tx).await?;
    let global = sqlx::query("UPDATE auth_login_limits SET attempts=CASE WHEN window_ends_at<=?1 THEN 1 ELSE attempts+1 END,window_ends_at=CASE WHEN window_ends_at<=?1 THEN ?2 ELSE window_ends_at END WHERE account_key='global' AND (window_ends_at<=?1 OR attempts<120)")
        .bind(&encoded).bind(time::encode(now + Duration::seconds(60)))
        .execute(&mut *tx).await?.rows_affected() == 1;
    if !global {
        tx.commit().await?;
        return Ok(false);
    }
    sqlx::query("DELETE FROM auth_login_limits WHERE account_key IN (SELECT account_key FROM auth_login_limits WHERE account_key<>'global' AND window_ends_at<=?1 ORDER BY window_ends_at LIMIT 32)")
        .bind(&encoded).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO auth_login_limits(account_key,attempts,window_ends_at) VALUES (?1,0,?2) ON CONFLICT DO NOTHING")
        .bind(account_key).bind(&encoded).execute(&mut *tx).await?;
    let account = sqlx::query("UPDATE auth_login_limits SET attempts=CASE WHEN window_ends_at<=?2 THEN 1 ELSE attempts+1 END,window_ends_at=CASE WHEN window_ends_at<=?2 THEN ?3 ELSE window_ends_at END WHERE account_key=?1 AND (window_ends_at<=?2 OR attempts<10)")
        .bind(account_key).bind(&encoded).bind(time::encode(now + Duration::seconds(300)))
        .execute(&mut *tx).await?.rows_affected() == 1;
    // Account denial still spends the global budget. Success never resets it.
    tx.commit().await?;
    Ok(account)
}
