use crate::backend::db::{
    models::{OrgMembership, OrgRole},
    organizations::*,
};
use anyhow::Result;
use sqlx::SqlitePool;
use OrganizationOutcome::{Applied, Conflict, Denied, Missing};

use super::authority::*;

pub async fn get_organization_role(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<OrgRole>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    tx.commit().await?;
    Ok(Applied(role))
}
pub async fn count_organization_members(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<i64>> {
    let mut tx = begin_read(pool).await?;
    require_read_org!(tx, actor, org);
    let count = sqlx::query_scalar("SELECT COUNT(*) FROM org_memberships WHERE org_id=?1")
        .bind(org)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(count))
}

pub async fn list_organization_members(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<OrgMembership>>> {
    let mut tx = begin_read(pool).await?;
    require_read_org!(tx, actor, org);
    let rows = sqlx::query_as(
        "SELECT * FROM org_memberships WHERE org_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
    )
    .bind(org)
    .bind(page.after)
    .bind(page.limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
pub async fn add_organization_member(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    target: i64,
    role: OrgRole,
) -> Result<OrganizationOutcome<OrgMembership>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let actor_role = require_org!(tx, actor, org);
    if !can_change_member(actor_role, None, Some(role)) {
        return Ok(Denied);
    }
    if !active(&mut tx, target).await? {
        return Ok(Denied);
    }
    if membership_role(&mut tx, org, target).await?.is_some() {
        return Ok(Conflict);
    }
    let member = sqlx::query_as(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES(?1,?2,?3) RETURNING *",
    )
    .bind(org)
    .bind(target)
    .bind(role.as_str())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Applied(member))
}
pub async fn update_organization_member_role(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    target: i64,
    role: OrgRole,
) -> Result<OrganizationOutcome<OrgMembership>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let actor_role = require_org!(tx, actor, org);
    if !administers(actor_role) {
        return Ok(Denied);
    }
    let target_active = active(&mut tx, target).await?;
    let Some(old) = membership_role(&mut tx, org, target).await? else {
        return Ok(Missing);
    };
    // A role this build does not recognize is refused; it cannot be changed here.
    let Ok(old) = old.parse::<OrgRole>() else {
        return Ok(Denied);
    };
    if !can_change_member(actor_role, Some(old), Some(role)) {
        return Ok(Denied);
    }
    // Inactive targets get no new authority: only an owner may lower an owner or admin to member.
    if !target_active
        && !(actor_role == OrgRole::Owner
            && role == OrgRole::Member
            && matches!(old, OrgRole::Owner | OrgRole::Admin))
    {
        return Ok(Denied);
    }
    if old == OrgRole::Owner
        && role != OrgRole::Owner
        && !remaining_active_owner(&mut tx, org, target).await?
    {
        return Ok(Conflict);
    }
    let member = sqlx::query_as(
        "UPDATE org_memberships SET role=?3 WHERE org_id=?1 AND user_id=?2 RETURNING *",
    )
    .bind(org)
    .bind(target)
    .bind(role.as_str())
    .fetch_one(&mut *tx)
    .await?;
    if (old == OrgRole::Owner && role != OrgRole::Owner)
        || (old == OrgRole::Admin && role == OrgRole::Member)
    {
        cancel_actor(&mut tx, org, target).await?;
    }
    tx.commit().await?;
    Ok(Applied(member))
}
pub async fn remove_organization_member(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let actor_role = require_org!(tx, actor, org);
    if !administers(actor_role) {
        return Ok(Denied);
    }
    active(&mut tx, target).await?;
    let Some(old) = membership_role(&mut tx, org, target).await? else {
        return Ok(Missing);
    };
    // A role this build does not recognize is refused; it cannot be removed here.
    let Ok(old) = old.parse::<OrgRole>() else {
        return Ok(Denied);
    };
    if !can_change_member(actor_role, Some(old), None) {
        return Ok(Denied);
    }
    if old == OrgRole::Owner && !remaining_active_owner(&mut tx, org, target).await? {
        return Ok(Conflict);
    }
    sqlx::query("DELETE FROM team_memberships WHERE user_id=?1 AND team_id IN (SELECT id FROM teams WHERE org_id=?2)")
        .bind(target).bind(org).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM repo_access WHERE user_id=?1 AND repo_id IN (SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=?2)")
        .bind(target).bind(org).execute(&mut *tx).await?;
    cancel_actor(&mut tx, org, target).await?;
    sqlx::query("DELETE FROM org_memberships WHERE org_id=?1 AND user_id=?2")
        .bind(org)
        .bind(target)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(()))
}
