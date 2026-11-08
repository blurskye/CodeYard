use crate::organizations::access::OrganizationRepository;
use crate::organizations::OrganizationPage;
use crate::sqlite::organizations::authority::organization_repo_access_in_transaction;
use anyhow::Result;
use codeyard_database::models::AccessLevel;
use codeyard_database::models::Repository;
use codeyard_database::pool::SqlitePool;
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_database::queries::sqlite::organizations::authority::begin_read;
use codeyard_database::require_read_org_sqlite;
use OrganizationOutcome::Applied;

pub async fn get_organization_repo_access(
    pool: &SqlitePool,
    user: i64,
    repository: &Repository,
) -> Result<AccessLevel> {
    let mut tx = begin_read(pool).await?;
    let result = organization_repo_access_in_transaction(&mut tx, user, repository).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn list_organization_profile_repositories(
    pool: &SqlitePool,
    org: i64,
    viewer: Option<i64>,
    page: OrganizationPage,
) -> Result<Vec<Repository>> {
    let readable = codeyard_database::queries::sqlite::repositories::access::readable_condition();
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT r.* FROM repositories r LEFT JOIN users u ON u.id=?2 WHERE r.owner_kind='org' AND r.owner_user_id IS NULL AND r.owner_org_id=?1 AND {readable} ORDER BY r.name, r.id LIMIT ?4 OFFSET ?3")))
        .bind(org).bind(viewer).bind(page.after).bind(page.limit).fetch_all(pool).await?)
}

pub async fn count_organization_profile_repositories(
    pool: &SqlitePool,
    org: i64,
    viewer: Option<i64>,
) -> Result<i64> {
    let readable = codeyard_database::queries::sqlite::repositories::access::readable_condition();
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM repositories r LEFT JOIN users u ON u.id=?2 WHERE r.owner_kind='org' AND r.owner_user_id IS NULL AND r.owner_org_id=?1 AND {readable}")))
        .bind(org).bind(viewer).fetch_one(pool).await?)
}
pub async fn list_organization_repositories(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<OrganizationRepository>>> {
    let mut tx = begin_read(pool).await?;
    require_read_org_sqlite!(tx, actor, org);
    let readable = codeyard_database::queries::sqlite::repositories::access::readable_condition();
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT r.id, o.name AS owner, r.name FROM repositories r JOIN organizations o ON o.id=r.owner_org_id JOIN users u ON u.id=?3 WHERE r.owner_kind='org' AND r.owner_user_id IS NULL AND r.owner_org_id=?1 AND r.id>?2 AND {readable} ORDER BY r.id LIMIT ?4")))
        .bind(org)
        .bind(page.after)
        .bind(actor)
        .bind(page.limit)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
