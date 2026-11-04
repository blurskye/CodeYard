use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use sqlx::SqlitePool;

pub async fn verified_at(pool: &SqlitePool, input_hash: &str) -> Result<Option<DateTime<Utc>>> {
    Ok(
        sqlx::query_scalar("SELECT verified_at FROM signature_verifications WHERE input_hash=?1")
            .bind(input_hash)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn record_verified(pool: &SqlitePool, input_hash: &str) -> Result<DateTime<Utc>> {
    Ok(sqlx::query_scalar("INSERT INTO signature_verifications(input_hash) VALUES(?1) ON CONFLICT(input_hash) DO UPDATE SET input_hash=EXCLUDED.input_hash RETURNING verified_at")
        .bind(input_hash).fetch_one(pool).await?)
}
