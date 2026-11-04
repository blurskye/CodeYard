//! Bounded native inventory pages; REST's unpaged contract is intentionally separate.
use crate::backend::db::artifact_metadata::Record;
use crate::backend::db::artifact_metadata::Refusal;
use anyhow::Result;
use codeyard_db_core::DbPool;

#[derive(sqlx::FromRow)]
pub struct PageRecord {
    #[sqlx(flatten)]
    pub record: Record,
    pub repository: String,
}

pub async fn list(
    pool: &DbPool,
    session: Option<&str>,
    org: i64,
    digest: &str,
    kind: &str,
    after: i64,
) -> Result<Vec<PageRecord>> {
    anyhow::ensure!(
        (0..i64::MAX).contains(&after)
            && matches!(kind, "deployment" | "storage")
            && digest.starts_with("sha256:")
            && crate::backend::db::attestations::valid_digest(digest),
        Refusal::Invalid
    );
    match pool {
        codeyard_db_core::DbPool::Postgres(pool) => {
            crate::backend::db::postgres::artifact_metadata::pages::list(
                pool, session, org, digest, kind, after,
            )
            .await
        }
        codeyard_db_core::DbPool::Sqlite(pool) => {
            crate::backend::db::sqlite::artifact_metadata::pages::list(
                pool, session, org, digest, kind, after,
            )
            .await
        }
    }
}
