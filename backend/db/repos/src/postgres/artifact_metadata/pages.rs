use crate::artifact_metadata::pages::PageRecord;
use anyhow::Result;
use codeyard_database::pool::PgPool;

pub async fn list(
    pool: &PgPool,
    session: Option<&str>,
    org: i64,
    digest: &str,
    kind: &str,
    after: i64,
) -> Result<Vec<PageRecord>> {
    Ok(sqlx::query_as(&format!("SELECT d.id,a.repo_id,d.kind,a.digest,a.name,a.version,d.document,d.attestation_id,d.created_at,d.updated_at,r.name AS repository FROM artifact_metadata_records d JOIN artifact_metadata a ON a.id=d.artifact_id JOIN repositories r ON r.id=a.repo_id LEFT JOIN sessions s ON s.id=$1 AND s.expires_at>NOW() LEFT JOIN users u ON u.id=s.user_id AND NOT u.is_banned AND NOT u.must_change_password WHERE d.org_id=$2 AND r.owner_kind='org' AND r.owner_org_id=$2 AND a.digest=$3 AND d.kind=$4 AND d.id>$5 AND {} ORDER BY d.id LIMIT 31",codeyard_database::queries::postgres::repositories::access::readable_condition()))
        .bind(session).bind(org).bind(digest).bind(kind).bind(after).fetch_all(pool).await?)
}
