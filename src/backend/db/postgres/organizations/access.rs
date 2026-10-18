use crate::backend::db::{
    models::{AccessLevel, Repository},
    organizations::*,
};
use anyhow::Result;
use sqlx::PgPool;
use OrganizationOutcome::Applied;

use super::authority::*;

pub async fn get_organization_repo_access(
    pool: &PgPool,
    user: i64,
    repository: &Repository,
) -> Result<AccessLevel> {
    let mut tx = begin_read(pool).await?;
    let result = organization_repo_access_in_transaction(&mut tx, user, repository).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn list_organization_profile_repositories(
    pool: &PgPool,
    org: i64,
    viewer: Option<i64>,
) -> Result<Vec<Repository>> {
    // The private-repository rule is the one list_organization_repositories uses, without paging.
    Ok(sqlx::query_as("SELECT r.* FROM repositories r WHERE r.owner_kind='org' AND r.owner_user_id IS NULL AND r.owner_org_id=$1 AND (NOT r.is_private OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=r.owner_org_id AND m.user_id=$2 AND m.role IN ('owner','admin')) OR EXISTS(SELECT 1 FROM repo_access d WHERE d.repo_id=r.id AND d.user_id=$2 AND d.access_level BETWEEN 1 AND 5) OR EXISTS(SELECT 1 FROM team_repo_access g JOIN teams t ON t.id=g.team_id AND t.org_id=r.owner_org_id JOIN team_memberships tm ON tm.team_id=t.id AND tm.user_id=$2 AND tm.role IN ('member','maintainer') WHERE g.repo_id=r.id AND g.permission BETWEEN 1 AND 5 AND t.privacy IN ('visible','secret'))) ORDER BY r.name")
        .bind(org)
        .bind(viewer)
        .fetch_all(pool)
        .await?)
}
pub async fn list_organization_repositories(
    pool: &PgPool,
    actor: i64,
    org: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<OrganizationRepository>>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    // READ filter before LIMIT: admins, public repositories, direct access, or a visible team grant.
    let rows = sqlx::query_as("SELECT r.id, o.name AS owner, r.name FROM repositories r JOIN organizations o ON o.id=r.owner_org_id WHERE r.owner_kind='org' AND r.owner_user_id IS NULL AND r.owner_org_id=$1 AND r.id>$2 AND ($3 OR NOT r.is_private OR EXISTS(SELECT 1 FROM repo_access d WHERE d.repo_id=r.id AND d.user_id=$4 AND d.access_level BETWEEN 1 AND 5) OR EXISTS(SELECT 1 FROM team_repo_access g JOIN teams t ON t.id=g.team_id AND t.org_id=r.owner_org_id JOIN team_memberships tm ON tm.team_id=t.id AND tm.user_id=$4 AND tm.role IN ('member','maintainer') WHERE g.repo_id=r.id AND g.permission BETWEEN 1 AND 5 AND t.privacy IN ('visible','secret'))) ORDER BY r.id LIMIT $5")
        .bind(org)
        .bind(page.after)
        .bind(administers(role))
        .bind(actor)
        .bind(page.limit)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
