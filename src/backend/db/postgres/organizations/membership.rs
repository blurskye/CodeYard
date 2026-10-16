use crate::backend::db::{
    models::{OrgMembership, OrgRole},
    organizations::*,
};
use anyhow::Result;
use sqlx::PgPool;
use OrganizationOutcome::{Applied, Conflict, Denied, Missing};

use super::authority::*;

pub async fn get_organization_role(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<OrgRole>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    tx.commit().await?;
    Ok(Applied(role))
}
pub async fn count_organization_members(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<i64>> {
    let mut tx = begin_read(pool).await?;
    require_read_org!(tx, actor, org);
    let count = sqlx::query_scalar("SELECT COUNT(*) FROM org_memberships WHERE org_id=$1")
        .bind(org)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(count))
}

pub async fn list_organization_members(
    pool: &PgPool,
    actor: i64,
    org: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<OrgMembership>>> {
    let mut tx = begin_read(pool).await?;
    require_read_org!(tx, actor, org);
    let rows = sqlx::query_as(
        "SELECT * FROM org_memberships WHERE org_id=$1 AND id>$2 ORDER BY id LIMIT $3",
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
    pool: &PgPool,
    actor: i64,
    org: i64,
    target: i64,
    role: OrgRole,
) -> Result<OrganizationOutcome<OrgMembership>> {
    let mut tx = begin(pool).await?;
    crate::backend::db::postgres::users::transitions::lock_membership(&mut tx).await?;
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
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,$3) RETURNING *",
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
    pool: &PgPool,
    actor: i64,
    org: i64,
    target: i64,
    role: OrgRole,
) -> Result<OrganizationOutcome<OrgMembership>> {
    let mut tx = begin(pool).await?;
    crate::backend::db::postgres::users::transitions::lock_membership(&mut tx).await?;
    let actor_role = require_org!(tx, actor, org);
    if !administers(actor_role) {
        return Ok(Denied);
    }
    let target_active = active(&mut tx, target).await?;
    let Some(old) = membership_role(&mut tx, org, target).await? else {
        return Ok(Missing);
    };
    let old = old.parse::<OrgRole>().ok();
    // Unknown TARGET roles may be freshly assigned only by a known active owner.
    // Unknown ACTOR roles never pass authority; admins cannot use repair to escalate.
    if old.is_none() {
        if actor_role != OrgRole::Owner {
            return Ok(Denied);
        }
    } else if !can_change_member(actor_role, old, Some(role)) {
        return Ok(Denied);
    }
    // No new authority for inactive targets; only known owner/admin -> member.
    if !target_active
        && !(actor_role == OrgRole::Owner
            && role == OrgRole::Member
            && matches!(old, Some(OrgRole::Owner | OrgRole::Admin)))
    {
        return Ok(Denied);
    }
    if old == Some(OrgRole::Owner)
        && role != OrgRole::Owner
        && !remaining_active_owner(&mut tx, org, target).await?
    {
        return Ok(Conflict);
    }
    let member = sqlx::query_as(
        "UPDATE org_memberships SET role=$3 WHERE org_id=$1 AND user_id=$2 RETURNING *",
    )
    .bind(org)
    .bind(target)
    .bind(role.as_str())
    .fetch_one(&mut *tx)
    .await?;
    if old.is_none()
        || (old == Some(OrgRole::Owner) && role != OrgRole::Owner)
        || (old == Some(OrgRole::Admin) && role == OrgRole::Member)
    {
        cancel_actor(&mut tx, org, target).await?;
    }
    tx.commit().await?;
    Ok(Applied(member))
}
pub async fn remove_organization_member(
    pool: &PgPool,
    actor: i64,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = begin(pool).await?;
    crate::backend::db::postgres::users::transitions::lock_membership(&mut tx).await?;
    let actor_role = require_org!(tx, actor, org);
    if !administers(actor_role) {
        return Ok(Denied);
    }
    // Lock target activity even though cleanup does not require it to be active.
    active(&mut tx, target).await?;
    let Some(old) = membership_role(&mut tx, org, target).await? else {
        return Ok(Missing);
    };
    let old = old.parse::<OrgRole>().ok();
    if old.is_none() {
        if actor_role != OrgRole::Owner {
            return Ok(Denied);
        }
    } else if !can_change_member(actor_role, old, None) {
        return Ok(Denied);
    }
    if old == Some(OrgRole::Owner) && !remaining_active_owner(&mut tx, org, target).await? {
        return Ok(Conflict);
    }
    sqlx::query("DELETE FROM team_memberships WHERE user_id=$1 AND team_id IN (SELECT id FROM teams WHERE org_id=$2)")
        .bind(target).bind(org).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM repo_access WHERE user_id=$1 AND repo_id IN (SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=$2)")
        .bind(target).bind(org).execute(&mut *tx).await?;
    cancel_actor(&mut tx, org, target).await?;
    sqlx::query("DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2")
        .bind(org)
        .bind(target)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(()))
}
