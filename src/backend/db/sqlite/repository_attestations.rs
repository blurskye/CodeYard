use super::super::attestation_validation::{
    self as attestations, Attestation, AttestationError, Page,
};
use sqlx::SqlitePool as Pool;

pub async fn list(
    pool: &Pool,
    actor: Option<i64>,
    repo: i64,
    digest: &str,
    page: &Page<'_>,
) -> Result<(Vec<Attestation>, bool), AttestationError> {
    page.validate()?;
    let digest = attestations::subject_digest(digest)?;
    let mut tx = pool.begin().await?;
    let visible: bool = sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r LEFT JOIN users u ON u.id=$1 WHERE r.id=$2 AND {})",super::repositories::access::readable_condition())).bind(actor).bind(repo).fetch_one(&mut *tx).await?;
    if !visible {
        return Err(AttestationError::NotFound);
    }
    let mut rows: Vec<Attestation> = sqlx::query_as(&format!(r#"
        SELECT a.id,a.bundle,a.initiator FROM repo_attestations a
        JOIN attestation_subjects s ON s.attestation_id=a.id
        JOIN repositories r ON r.id=a.repo_id LEFT JOIN users u ON u.id=$1
        WHERE a.repo_id=$2 AND s.digest=$3 AND {}
          AND (CAST($4 AS BIGINT) IS NULL OR a.id<$4)
          AND (CAST($5 AS BIGINT) IS NULL OR a.id>$5)
          AND (CAST($6 AS TEXT) IS NULL OR a.predicate_type=$6
            OR ($6='provenance' AND a.predicate_type LIKE 'https://slsa.dev/provenance/%')
            OR ($6='sbom' AND (a.predicate_type LIKE 'https://spdx.dev/Document%' OR a.predicate_type LIKE 'https://cyclonedx.org/bom%'))
            OR ($6='release' AND (a.predicate_type LIKE 'https://in-toto.io/attestation/release/%' OR a.predicate_type LIKE 'https://github.com/attestations/release/%')))
        ORDER BY CASE WHEN CAST($5 AS BIGINT) IS NULL THEN a.id END DESC,
            CASE WHEN CAST($5 AS BIGINT) IS NOT NULL THEN a.id END ASC LIMIT $7
    "#,super::repositories::access::readable_condition()))
        .bind(actor).bind(repo).bind(digest).bind(page.after).bind(page.before).bind(page.predicate_type).bind(page.limit+1).fetch_all(&mut *tx).await?;
    let more = rows.len() > page.limit as usize;
    rows.truncate(page.limit as usize);
    if page.before.is_some() {
        rows.reverse();
    }
    tx.commit().await?;
    Ok((rows, more))
}

pub async fn get(
    pool: &Pool,
    actor: Option<i64>,
    repo: i64,
    id: i64,
) -> Result<Attestation, AttestationError> {
    sqlx::query_as(&format!("SELECT a.id,a.bundle,a.initiator FROM repo_attestations a JOIN repositories r ON r.id=a.repo_id LEFT JOIN users u ON u.id=$1 WHERE a.repo_id=$2 AND a.id=$3 AND {}",super::repositories::access::readable_condition()))
        .bind(actor).bind(repo).bind(id).fetch_optional(pool).await?.ok_or(AttestationError::NotFound)
}
