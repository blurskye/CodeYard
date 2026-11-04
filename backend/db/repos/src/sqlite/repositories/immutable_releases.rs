use crate::releases::ReleaseRefusal;
use crate::repositories::immutable_releases::Publication;
use crate::repositories::immutable_releases::Refusal;
use crate::repositories::immutable_releases::Reservation;
use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::models::Release;
use codeyard_model::release_integrity::Artifact;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;

// Enforced when the repository opts in or its organization enforces it for all or selected
// repositories. A repository setting never removes an organization's enforcement.
const OWNER_ENFORCED_SQL: &str = "EXISTS(SELECT 1 FROM repositories r JOIN organization_immutable_releases o ON o.org_id=r.owner_org_id WHERE r.id=?1 AND (o.enforced_repositories='all' OR (o.enforced_repositories='selected' AND EXISTS(SELECT 1 FROM organization_immutable_release_repositories s WHERE s.org_id=o.org_id AND s.repo_id=r.id))))";
fn enforced_sql() -> String {
    format!("(EXISTS(SELECT 1 FROM repository_immutable_releases WHERE repo_id=?1) OR {OWNER_ENFORCED_SQL})")
}
/// Whether the repository opts in, and whether its owner enforces immutable releases.
pub async fn policy(pool: &SqlitePool, repo: i64) -> Result<(bool, bool)> {
    Ok(sqlx::query_as(&format!("SELECT EXISTS(SELECT 1 FROM repository_immutable_releases WHERE repo_id=?1), {OWNER_ENFORCED_SQL}")).bind(repo).fetch_one(pool).await?)
}
pub async fn enabled(pool: &SqlitePool, repo: i64) -> Result<bool> {
    Ok(sqlx::query_scalar(&format!("SELECT {}", enforced_sql()))
        .bind(repo)
        .fetch_one(pool)
        .await?)
}
pub async fn set_enabled(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo: i64,
    enabled: bool,
) -> Result<()> {
    let credential = actor.into();
    let actor = credential.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_archive_authority(
            &mut tx, actor, repo
        )
        .await?,
        Refusal::Denied
    );
    ensure!(
        codeyard_db_core::queries::sqlite::credentials::writes(
            &mut tx,
            &credential,
            Some(repo),
            None,
            "administration"
        )
        .await?,
        Refusal::Denied
    );
    let archived: bool = sqlx::query_scalar("SELECT is_archived FROM repositories WHERE id=?1")
        .bind(repo)
        .fetch_one(&mut *tx)
        .await?;
    ensure!(!archived, Refusal::Archived);
    if enabled {
        sqlx::query(
            "INSERT INTO repository_immutable_releases(repo_id) VALUES(?1) ON CONFLICT DO NOTHING",
        )
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query("DELETE FROM repository_immutable_releases WHERE repo_id=?1")
            .bind(repo)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn reservations(
    pool: &SqlitePool,
    repo: i64,
    owner: &str,
    name: &str,
    tags: &[String],
) -> Result<Vec<Reservation>> {
    Ok(sqlx::query_as("WITH scoped(repo_id) AS (SELECT ?1 UNION SELECT n.repo_id FROM immutable_release_repository_names n JOIN immutable_release_owner_names o ON o.owner_kind=n.owner_kind AND o.owner_id=n.owner_id WHERE o.name_key=LOWER(?2) AND n.name_key=LOWER(?3)) SELECT p.repo_id,p.tag_name,p.tag_oid,p.release_id FROM immutable_release_publications p JOIN scoped s ON s.repo_id=p.repo_id WHERE p.tag_name IN(SELECT value FROM json_each(?4)) LIMIT 10001").bind(repo).bind(owner).bind(name).bind(serde_json::to_string(tags)?).fetch_all(pool).await?)
}
pub async fn assets(pool: &SqlitePool, release: i64) -> Result<Vec<Artifact>> {
    Ok(sqlx::query_as("SELECT a.name,a.size,f.sha256_hex AS sha256 FROM release_assets a JOIN files f ON f.id=a.file_id WHERE a.release_id=?1 LIMIT 1001").bind(release).fetch_all(pool).await?)
}
pub async fn attestation(
    pool: &SqlitePool,
    repo: i64,
    release: i64,
) -> Result<Option<(String, String)>> {
    Ok(sqlx::query_as("SELECT signer_key,attestation FROM immutable_release_publications WHERE repo_id=?1 AND release_id=?2").bind(repo).bind(release).fetch_optional(pool).await?)
}
/// A deleted release's tag name cannot be assigned to a new release, even with the policy off.
pub(crate) async fn assert_available(
    tx: &mut Transaction<'_, Sqlite>,
    repo: i64,
    tag: &str,
) -> Result<()> {
    let reserved:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM immutable_release_publications p WHERE p.tag_name=?2 AND (p.repo_id=?1 OR EXISTS(SELECT 1 FROM immutable_release_repository_names n JOIN immutable_release_owner_names o ON o.owner_kind=n.owner_kind AND o.owner_id=n.owner_id JOIN repositories r ON r.id=?1 LEFT JOIN users u ON u.id=r.owner_user_id LEFT JOIN organizations g ON g.id=r.owner_org_id WHERE n.repo_id=p.repo_id AND n.name_key=LOWER(r.name) AND o.name_key=LOWER(COALESCE(u.username,g.name)))))").bind(repo).bind(tag).fetch_one(&mut **tx).await?;
    ensure!(!reserved, ReleaseRefusal::TagReserved);
    Ok(())
}
pub(crate) async fn freeze(
    tx: &mut Transaction<'_, Sqlite>,
    release: &Release,
    was_draft: bool,
    publication: Option<&Publication>,
) -> Result<()> {
    if !was_draft || release.is_draft || release.is_immutable {
        return Ok(());
    }
    let enabled: bool = sqlx::query_scalar(&format!("SELECT {}", enforced_sql()))
        .bind(release.repo_id)
        .fetch_one(&mut **tx)
        .await?;
    if !enabled {
        return Ok(());
    }
    let publication = publication.ok_or(ReleaseRefusal::Conflict)?;
    let (owner, name): (String, String) = sqlx::query_as("SELECT COALESCE(u.username,o.name),r.name FROM repositories r LEFT JOIN users u ON u.id=r.owner_user_id LEFT JOIN organizations o ON o.id=r.owner_org_id WHERE r.id=?1").bind(release.repo_id).fetch_one(&mut **tx).await?;
    let mut assets:Vec<Artifact>=sqlx::query_as("SELECT a.name,a.size,f.sha256_hex AS sha256 FROM release_assets a JOIN files f ON f.id=a.file_id WHERE a.release_id=?1 LIMIT 1001").bind(release.id).fetch_all(&mut **tx).await?;
    assets.sort_by(|a, b| a.name.cmp(&b.name));
    publication.matches(
        release.repo_id,
        &release.tag_name,
        &assets,
        &codeyard_model::urls::namespace_url(&owner, &name),
    )?;
    let inactive:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM release_assets a JOIN files f ON f.id=a.file_id WHERE a.release_id=?1 AND NOT f.is_active)").bind(release.id).fetch_one(&mut **tx).await?;
    ensure!(!inactive, ReleaseRefusal::Conflict);
    sqlx::query("UPDATE releases SET is_immutable=1,target_commitish=?2 WHERE id=?1")
        .bind(release.id)
        .bind(&publication.statement.predicate.commit_sha)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO immutable_release_publications(repo_id,tag_name,tag_oid,commit_sha,release_id,signer_key,attestation) VALUES(?1,?2,?3,?4,?5,?6,?7)").bind(release.repo_id).bind(&release.tag_name).bind(&publication.statement.predicate.tag_oid).bind(&publication.statement.predicate.commit_sha).bind(release.id).bind(&publication.signer_key).bind(&publication.attestation).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO immutable_release_repository_names(repo_id,owner_kind,owner_id,name_key) SELECT id,owner_kind,COALESCE(owner_user_id,owner_org_id),LOWER(name) FROM repositories WHERE id=?1 ON CONFLICT DO NOTHING").bind(release.repo_id).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO immutable_release_owner_names(owner_kind,owner_id,name_key) SELECT r.owner_kind,COALESCE(r.owner_user_id,r.owner_org_id),LOWER(COALESCE(u.username,g.name)) FROM repositories r LEFT JOIN users u ON u.id=r.owner_user_id LEFT JOIN organizations g ON g.id=r.owner_org_id WHERE r.id=?1 ON CONFLICT DO NOTHING").bind(release.repo_id).execute(&mut **tx).await?;
    Ok(())
}
