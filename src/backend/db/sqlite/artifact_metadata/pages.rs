use crate::backend::db::artifact_metadata::Record;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn list(
    pool: &SqlitePool,
    viewer: Option<i64>,
    org: i64,
    digest: &str,
    kind: &str,
    after: i64,
) -> Result<Vec<Record>> {
    Ok(sqlx::query_as(&format!("SELECT d.id,a.repo_id,d.kind,a.digest,a.name,a.version,d.document,d.attestation_id,d.created_at,d.updated_at FROM artifact_metadata_records d JOIN artifact_metadata a ON a.id=d.artifact_id JOIN repositories r ON r.id=a.repo_id LEFT JOIN users u ON u.id=?1 WHERE d.org_id=?2 AND r.owner_kind='org' AND r.owner_org_id=?2 AND a.digest=?3 AND d.kind=?4 AND d.id>?5 AND {} ORDER BY d.id LIMIT 31",super::super::repositories::access::readable_condition()))
        .bind(viewer).bind(org).bind(digest).bind(kind).bind(after).fetch_all(pool).await?)
}
