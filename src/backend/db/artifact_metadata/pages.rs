//! Bounded native inventory pages; REST's unpaged contract is intentionally separate.
use crate::backend::db::{
    artifact_metadata::{Record, Refusal},
    DbPool,
};
use anyhow::Result;

pub async fn list(
    pool: &DbPool,
    viewer: Option<i64>,
    org: i64,
    digest: &str,
    kind: &str,
    after: i64,
) -> Result<Vec<Record>> {
    anyhow::ensure!(
        (0..i64::MAX).contains(&after)
            && matches!(kind, "deployment" | "storage")
            && digest.starts_with("sha256:")
            && crate::backend::db::attestations::valid_digest(digest),
        Refusal::Invalid
    );
    match pool {
        DbPool::Postgres(pool) => {
            crate::backend::db::postgres::artifact_metadata::pages::list(
                pool, viewer, org, digest, kind, after,
            )
            .await
        }
        DbPool::Sqlite(pool) => {
            crate::backend::db::sqlite::artifact_metadata::pages::list(
                pool, viewer, org, digest, kind, after,
            )
            .await
        }
    }
}
