use anyhow::{ensure, Result};
use sqlx::{Postgres, Transaction};

/// Owner-scoped association changes precede repository and metadata row locks.
pub async fn lock(tx: &mut Transaction<'_, Postgres>, kind: &str, owner: i64) -> Result<()> {
    ensure!(matches!(kind, "user" | "org"), "invalid attestation owner");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(COALESCE(current_schema(),'') || ':attestations:' || $1 || ':' || $2::bigint::text,0))")
        .bind(kind).bind(owner).execute(&mut **tx).await?;
    Ok(())
}

/// Resolve before repository locks, then verify the same owner after admission locks it.
pub async fn repository_owner(
    tx: &mut Transaction<'_, Postgres>,
    repo: i64,
) -> Result<(String, i64)> {
    Ok(sqlx::query_as(
        "SELECT owner_kind,COALESCE(owner_user_id,owner_org_id) FROM repositories WHERE id=$1",
    )
    .bind(repo)
    .fetch_one(&mut **tx)
    .await?)
}
