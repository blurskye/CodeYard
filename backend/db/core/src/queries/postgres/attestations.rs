use anyhow::Result;
use sqlx::Postgres;
use sqlx::Transaction;
/// Take before repository rows: association removal can also update metadata foreign keys.
/// This serializes those parent/child changes with publishers and orphan collection.
pub async fn lock(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(723947122)")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
