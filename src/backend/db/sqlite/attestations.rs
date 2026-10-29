use crate::backend::db::attestations::{Attestation, Bundle, Filter, Owner, Refusal, Summary};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn collect(pool: &SqlitePool) -> Result<u64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let count = sqlx::query("DELETE FROM attestation_bundles WHERE hash IN (SELECT b.hash FROM attestation_bundles b WHERE NOT EXISTS(SELECT 1 FROM artifact_attestations a WHERE a.bundle_hash=b.hash) ORDER BY b.hash LIMIT 100)")
        .execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(count)
}

pub async fn create(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    bundle: &Bundle,
    initiator: &str,
) -> Result<i64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        super::discussions::lock_write_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    sqlx::query(
        "INSERT INTO attestation_bundles(hash,bundle) VALUES(?1,?2) ON CONFLICT(hash) DO NOTHING",
    )
    .bind(&bundle.hash)
    .bind(&bundle.json)
    .execute(&mut *tx)
    .await?;
    let id = sqlx::query_scalar("INSERT INTO artifact_attestations(repo_id,bundle_hash,predicate_type,initiator,created_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(repo_id,bundle_hash) DO UPDATE SET bundle_hash=excluded.bundle_hash RETURNING id")
        .bind(repo).bind(&bundle.hash).bind(&bundle.predicate).bind(initiator).bind(super::time::encode(super::time::now())).fetch_one(&mut *tx).await?;
    let subjects: Vec<_> = bundle.subjects.iter().collect();
    for subjects in subjects.chunks(400) {
        let mut insert = sqlx::QueryBuilder::<sqlx::Sqlite>::new("INSERT INTO attestation_subjects(attestation_id,digest) ");
        insert.push_values(subjects, |mut row, digest| { row.push_bind(id).push_bind(*digest); });
        insert.push(" ON CONFLICT DO NOTHING");
        insert.build().execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(id)
}

fn condition() -> String {
    format!("FROM artifact_attestations a JOIN repositories r ON r.id=a.repo_id LEFT JOIN users u ON u.id=?1 WHERE (?2 IS NULL OR r.owner_org_id=?2) AND (?9 IS NULL OR r.owner_user_id=?9) AND (?3 IS NULL OR r.id=?3) AND (?4 IS NULL OR EXISTS(SELECT 1 FROM attestation_subjects s WHERE s.attestation_id=a.id AND s.digest IN (SELECT value FROM json_each(?4)))) AND (?5 IS NULL OR a.predicate_type=?5 OR (?5='provenance' AND a.predicate_type LIKE 'https://slsa.dev/provenance/%') OR (?5='sbom' AND (a.predicate_type LIKE 'https://spdx.dev/Document%' OR a.predicate_type LIKE 'https://cyclonedx.org/bom%')) OR (?5='release' AND (a.predicate_type LIKE 'https://in-toto.io/attestation/release/%' OR a.predicate_type LIKE 'https://github.com/attestations/release/%'))) AND {}", super::repositories::access::readable_condition())
}

pub async fn list(pool: &SqlitePool, filter: &Filter<'_>) -> Result<Vec<Attestation>> {
    use futures_util::TryStreamExt;
    let sql = format!("SELECT a.id,a.repo_id,(SELECT b.bundle FROM attestation_bundles b WHERE b.hash=a.bundle_hash) AS bundle,a.predicate_type,a.initiator {} AND a.id>?6 AND a.id<?7 ORDER BY a.id {} LIMIT ?8", condition(), if filter.before < i64::MAX { "DESC" } else { "ASC" });
    let mut stream = sqlx::query_as::<_, Attestation>(&sql)
        .bind(filter.actor).bind(filter.owner.and_then(Owner::org)).bind(filter.repo).bind(filter.digests.map(serde_json::to_string).transpose()?).bind(filter.predicate).bind(filter.after).bind(filter.before).bind(filter.limit).bind(filter.owner.and_then(Owner::user)).fetch(pool);
    let mut rows = Vec::new();
    let mut bytes = 0;
    while let Some(row) = stream.try_next().await? {
        if !crate::backend::db::attestations::append_page(&mut rows, &mut bytes, row) { break; }
    }
    Ok(rows)
}

pub async fn summaries(pool: &SqlitePool, filter: &Filter<'_>) -> Result<Vec<Summary>> {
    Ok(sqlx::query_as(&format!("SELECT a.id,a.repo_id,a.predicate_type,(SELECT json_group_array(s.digest) FROM attestation_subjects s WHERE s.attestation_id=a.id) AS subjects {} AND a.id>?6 AND a.id<?7 ORDER BY a.id {} LIMIT ?8", condition(), if filter.before < i64::MAX { "DESC" } else { "ASC" }))
        .bind(filter.actor).bind(filter.owner.and_then(Owner::org)).bind(filter.repo).bind(filter.digests.map(serde_json::to_string).transpose()?).bind(filter.predicate).bind(filter.after).bind(filter.before).bind(filter.limit).bind(filter.owner.and_then(Owner::user)).fetch_all(pool).await?)
}

pub async fn repositories(pool: &SqlitePool, filter: &Filter<'_>) -> Result<Vec<(i64, String)>> {
    Ok(sqlx::query_as(&format!(
        "SELECT DISTINCT r.id,r.name {} AND r.id>?6 AND r.id<?7 ORDER BY r.id {} LIMIT ?8",
        condition(),
        if filter.before < i64::MAX {
            "DESC"
        } else {
            "ASC"
        }
    ))
    .bind(filter.actor)
    .bind(filter.owner.and_then(Owner::org))
    .bind(filter.repo)
    .bind(filter.digests.map(serde_json::to_string).transpose()?)
    .bind(filter.predicate)
    .bind(filter.after)
    .bind(filter.before)
    .bind(filter.limit)
    .bind(filter.owner.and_then(Owner::user))
    .fetch_all(pool)
    .await?)
}

pub async fn delete(
    pool: &SqlitePool,
    actor: i64,
    owner: Owner,
    ids: &[i64],
    digests: &[String],
) -> Result<u64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let rows: Vec<(i64,i64)> = sqlx::query_as(&format!("SELECT DISTINCT a.id,a.repo_id FROM artifact_attestations a JOIN repositories r ON r.id=a.repo_id JOIN users u ON u.id=?4 WHERE ((?1 IS NOT NULL AND r.owner_org_id=?1) OR (?5 IS NOT NULL AND r.owner_user_id=?5)) AND (a.id IN (SELECT value FROM json_each(?2)) OR EXISTS(SELECT 1 FROM attestation_subjects s WHERE s.attestation_id=a.id AND s.digest IN (SELECT value FROM json_each(?3)))) AND {} ORDER BY a.repo_id,a.id",super::repositories::access::readable_condition()))
        .bind(owner.org()).bind(serde_json::to_string(ids)?).bind(serde_json::to_string(digests)?).bind(actor).bind(owner.user()).fetch_all(&mut *tx).await?;
    for (_, repo) in &rows {
        anyhow::ensure!(
            super::discussions::lock_write_authority(&mut tx, actor, *repo).await?,
            Refusal::Denied
        );
    }
    let mut count = 0;
    for (id, _) in rows {
        count += sqlx::query("DELETE FROM artifact_attestations WHERE id=?1")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    tx.commit().await?;
    Ok(count)
}
