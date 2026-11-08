use crate::attestations::Attestation;
use crate::attestations::Bundle;
use crate::attestations::Filter;
use crate::attestations::Owner;
use crate::attestations::Refusal;
use crate::attestations::Summary;
use anyhow::Result;
use codeyard_db_core::pool::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;

async fn begin(pool: &PgPool) -> Result<Transaction<'_, Postgres>> {
    Ok(pool.begin().await?)
}

pub async fn collect(pool: &PgPool) -> Result<u64> {
    let mut tx = begin(pool).await?;
    let hashes: Vec<String> = sqlx::query_scalar("SELECT b.hash FROM attestation_bundles b WHERE NOT EXISTS(SELECT 1 FROM artifact_attestations a WHERE a.bundle_hash=b.hash) ORDER BY b.hash LIMIT 100 FOR UPDATE OF b SKIP LOCKED")
        .fetch_all(&mut *tx).await?;
    let count = sqlx::query("DELETE FROM attestation_bundles b WHERE hash=ANY($1) AND NOT EXISTS(SELECT 1 FROM artifact_attestations a WHERE a.bundle_hash=b.hash)")
        .bind(&hashes).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(count)
}

pub async fn create(
    pool: &PgPool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo: i64,
    bundle: &Bundle,
    initiator: &str,
) -> Result<i64> {
    let credential = actor.into();
    let actor = credential.user_id;
    let mut tx = begin(pool).await?;
    let owner =
        codeyard_db_core::queries::postgres::attestations::repository_owner(&mut tx, repo).await?;
    lock(&mut tx, &owner.0, owner.1).await?;
    anyhow::ensure!(
        codeyard_db_core::queries::postgres::discussions::lock_write_authority(
            &mut tx, actor, repo
        )
        .await?
            && codeyard_db_core::queries::postgres::credentials::repository(
                &mut tx,
                &credential,
                repo,
                "attestations",
                true
            )
            .await?,
        Refusal::Denied
    );
    anyhow::ensure!(
        codeyard_db_core::queries::postgres::attestations::repository_owner(&mut tx, repo).await?
            == owner,
        Refusal::Missing
    );
    // Retain the bundle row until its association commits; collection skips this publisher.
    sqlx::query(
        "INSERT INTO attestation_bundles(hash,bundle) VALUES($1,$2) ON CONFLICT(hash) DO UPDATE SET hash=excluded.hash",
    )
    .bind(&bundle.hash)
    .bind(&bundle.json)
    .execute(&mut *tx)
    .await?;
    let id = sqlx::query_scalar("INSERT INTO artifact_attestations(repo_id,bundle_hash,predicate_type,initiator,created_at) VALUES($1,$2,$3,$4,$5) ON CONFLICT(repo_id,bundle_hash) DO UPDATE SET bundle_hash=excluded.bundle_hash RETURNING id")
        .bind(repo).bind(&bundle.hash).bind(&bundle.predicate).bind(initiator).bind(chrono::Utc::now()).fetch_one(&mut *tx).await?;
    if !bundle.subjects.is_empty() {
        let mut insert = sqlx::QueryBuilder::<Postgres>::new(
            "INSERT INTO attestation_subjects(attestation_id,digest) ",
        );
        insert.push_values(&bundle.subjects, |mut row, digest| {
            row.push_bind(id).push_bind(digest);
        });
        insert.push(" ON CONFLICT DO NOTHING");
        insert.build().execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(id)
}

fn condition() -> String {
    format!("FROM artifact_attestations a JOIN repositories r ON r.id=a.repo_id LEFT JOIN users u ON u.id=$1 AND NOT u.is_banned AND NOT u.must_change_password AND (CAST($10 AS TEXT) IS NULL OR EXISTS(SELECT 1 FROM sessions s WHERE s.id=$10 AND s.user_id=u.id AND s.expires_at>NOW())) WHERE (CAST($2 AS BIGINT) IS NULL OR r.owner_org_id=$2) AND (CAST($9 AS BIGINT) IS NULL OR r.owner_user_id=$9) AND (CAST($3 AS BIGINT) IS NULL OR r.id=$3) AND (CAST($4 AS TEXT[]) IS NULL OR EXISTS(SELECT 1 FROM attestation_subjects s WHERE s.attestation_id=a.id AND s.digest=ANY($4))) AND (CAST($5 AS TEXT) IS NULL OR a.predicate_type=$5 OR ($5='provenance' AND a.predicate_type LIKE 'https://slsa.dev/provenance/%') OR ($5='sbom' AND (a.predicate_type LIKE 'https://spdx.dev/Document%' OR a.predicate_type LIKE 'https://cyclonedx.org/bom%')) OR ($5='release' AND (a.predicate_type LIKE 'https://in-toto.io/attestation/release/%' OR a.predicate_type LIKE 'https://github.com/attestations/release/%'))) AND {}", codeyard_db_core::queries::postgres::repositories::access::readable_condition())
}

pub async fn list(pool: &PgPool, filter: &Filter<'_>) -> Result<Vec<Attestation>> {
    use futures_util::TryStreamExt;
    let sql = format!("SELECT a.id,a.repo_id,(SELECT b.bundle FROM attestation_bundles b WHERE b.hash=a.bundle_hash) AS bundle,a.predicate_type,a.initiator {} AND a.id>$6 AND a.id<$7 ORDER BY a.id {} LIMIT $8", condition(), if filter.before < i64::MAX { "DESC" } else { "ASC" });
    let mut stream = sqlx::query_as::<_, Attestation>(&sql)
        .bind(filter.actor)
        .bind(filter.owner.and_then(Owner::org))
        .bind(filter.repo)
        .bind(filter.digests)
        .bind(filter.predicate)
        .bind(filter.after)
        .bind(filter.before)
        .bind(filter.limit)
        .bind(filter.owner.and_then(Owner::user))
        .bind(filter.session)
        .fetch(pool);
    let mut rows = Vec::new();
    let mut bytes = 0;
    while let Some(row) = stream.try_next().await? {
        if !crate::attestations::append_page(&mut rows, &mut bytes, row) {
            break;
        }
    }
    Ok(rows)
}

pub async fn summaries(pool: &PgPool, filter: &Filter<'_>) -> Result<Vec<Summary>> {
    Ok(sqlx::query_as(&format!("SELECT a.id,a.repo_id,r.name AS repository,r.owner_org_id,COALESCE((SELECT name FROM organizations WHERE id=r.owner_org_id),(SELECT username FROM users WHERE id=r.owner_user_id)) AS owner_name,COALESCE(NOT r.is_archived AND ({write_access})>=3,false) AS can_delete,a.predicate_type,(SELECT COALESCE(json_agg(s.digest ORDER BY s.digest),'[]')::text FROM attestation_subjects s WHERE s.attestation_id=a.id) AS subjects {} AND a.id>$6 AND a.id<$7 ORDER BY a.id {} LIMIT $8", condition(), if filter.before < i64::MAX { "DESC" } else { "ASC" }, write_access=codeyard_db_core::queries::postgres::repositories::access::USER_REPO_ACCESS_SQL))
        .bind(filter.actor).bind(filter.owner.and_then(Owner::org)).bind(filter.repo).bind(filter.digests).bind(filter.predicate).bind(filter.after).bind(filter.before).bind(filter.limit).bind(filter.owner.and_then(Owner::user)).bind(filter.session).fetch_all(pool).await?)
}

pub async fn repositories(pool: &PgPool, filter: &Filter<'_>) -> Result<Vec<(i64, String)>> {
    Ok(sqlx::query_as(&format!(
        "SELECT DISTINCT r.id,r.name {} AND r.id>$6 AND r.id<$7 ORDER BY r.id {} LIMIT $8",
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
    .bind(filter.digests)
    .bind(filter.predicate)
    .bind(filter.after)
    .bind(filter.before)
    .bind(filter.limit)
    .bind(filter.owner.and_then(Owner::user))
    .bind(filter.session)
    .fetch_all(pool)
    .await?)
}

pub async fn delete(
    pool: &PgPool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    owner: Owner,
    ids: &[i64],
    digests: &[String],
) -> Result<u64> {
    let credential = actor.into();
    let actor = credential.user_id;
    let mut tx = begin(pool).await?;
    match owner {
        Owner::Organization(id) => lock(&mut tx, "org", id).await?,
        Owner::User(id) => lock(&mut tx, "user", id).await?,
    }
    codeyard_db_identity::postgres::credentials::require_account(&mut tx, &credential)
        .await
        .map_err(|error| {
            if error.is::<codeyard_types::credentials::AccountRestricted>() {
                anyhow::Error::new(Refusal::Denied)
            } else {
                error
            }
        })?;
    let rows: Vec<(i64,i64)> = sqlx::query_as(&format!("SELECT DISTINCT a.id,a.repo_id FROM artifact_attestations a JOIN repositories r ON r.id=a.repo_id JOIN users u ON u.id=$4 WHERE ((CAST($1 AS BIGINT) IS NOT NULL AND r.owner_org_id=$1) OR (CAST($5 AS BIGINT) IS NOT NULL AND r.owner_user_id=$5)) AND (a.id=ANY($2) OR EXISTS(SELECT 1 FROM attestation_subjects s WHERE s.attestation_id=a.id AND s.digest=ANY($3))) AND {} ORDER BY a.repo_id,a.id",codeyard_db_core::queries::postgres::repositories::access::readable_condition()))
        .bind(owner.org()).bind(ids).bind(digests).bind(actor).bind(owner.user()).fetch_all(&mut *tx).await?;
    for (_, repo) in &rows {
        anyhow::ensure!(
            codeyard_db_core::queries::postgres::discussions::lock_write_authority(
                &mut tx, actor, *repo
            )
            .await?
                && codeyard_db_core::queries::postgres::credentials::repository(
                    &mut tx,
                    &credential,
                    *repo,
                    "attestations",
                    true
                )
                .await?,
            Refusal::Denied
        );
        let belongs: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE id=$1 AND ((owner_kind='org' AND owner_org_id=$2) OR (owner_kind='user' AND owner_user_id=$3)))").bind(repo).bind(owner.org()).bind(owner.user()).fetch_one(&mut *tx).await?;
        anyhow::ensure!(belongs, Refusal::Missing);
    }
    let mut count = 0;
    for (id, _) in rows {
        count += sqlx::query("DELETE FROM artifact_attestations WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    // Bundle garbage collection is separate: concurrent publishers may still reference a bundle.
    tx.commit().await?;
    Ok(count)
}

use codeyard_db_core::queries::postgres::attestations::lock;
