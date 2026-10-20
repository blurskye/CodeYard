use crate::backend::db::{
    models::{OrgInvitation, OrgMembership, OrgRole, ReceivedInvitation},
    organizations::*,
};
use anyhow::Result;
use sqlx::PgPool;
use OrganizationOutcome::{Applied, Conflict, Denied, Missing, Suspended};

use super::authority::*;

/// Records a pending invitation. A member, or someone already invited, is a Conflict.
pub async fn invite_organization_member(
    pool: &PgPool,
    actor: i64,
    org: i64,
    target: i64,
    role: OrgRole,
) -> Result<OrganizationOutcome<OrgInvitation>> {
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
    let invitation = sqlx::query_as(
        "INSERT INTO organization_invitations(org_id,user_id,role,invited_by_id) VALUES($1,$2,$3,$4) ON CONFLICT(org_id,user_id) DO NOTHING RETURNING *",
    )
    .bind(org)
    .bind(target)
    .bind(role.as_str())
    .bind(actor)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(invitation) = invitation else {
        return Ok(Conflict);
    };
    tx.commit().await?;
    Ok(Applied(invitation))
}

pub async fn list_organization_invitations(
    pool: &PgPool,
    actor: i64,
    org: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<OrgInvitation>>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    if !administers(role) {
        return Ok(Denied);
    }
    let rows = sqlx::query_as(
        "SELECT * FROM organization_invitations WHERE org_id=$1 AND id>$2 ORDER BY id LIMIT $3",
    )
    .bind(org)
    .bind(page.after)
    .bind(page.limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Applied(rows))
}

/// Withdraws a pending invitation. An administrator cannot withdraw an offer of a role
/// they could not have granted.
pub async fn cancel_organization_invitation(
    pool: &PgPool,
    actor: i64,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = begin(pool).await?;
    crate::backend::db::postgres::users::transitions::lock_membership(&mut tx).await?;
    let actor_role = require_org!(tx, actor, org);
    let Some(invited) = sqlx::query_scalar::<_, String>(
        "SELECT role FROM organization_invitations WHERE org_id=$1 AND user_id=$2",
    )
    .bind(org)
    .bind(target)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Missing);
    };
    let Ok(invited) = invited.parse::<OrgRole>() else {
        return Ok(Denied);
    };
    if !can_change_member(actor_role, None, Some(invited)) {
        return Ok(Denied);
    }
    sqlx::query("DELETE FROM organization_invitations WHERE org_id=$1 AND user_id=$2")
        .bind(org)
        .bind(target)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(()))
}

pub async fn list_received_organization_invitations(
    pool: &PgPool,
    actor: i64,
) -> Result<Vec<ReceivedInvitation>> {
    Ok(sqlx::query_as(
        "SELECT i.org_id,o.name AS org_name,o.display_name AS org_display_name,i.role,i.created_at FROM organization_invitations i JOIN organizations o ON o.id=i.org_id WHERE i.user_id=$1 ORDER BY i.id",
    )
    .bind(actor)
    .fetch_all(pool)
    .await?)
}

/// Turns the invitation into membership in one transaction. An inactive invitee, or an
/// organization with no active owner, cannot take the seat.
pub async fn accept_organization_invitation(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<OrgMembership>> {
    let mut tx = begin(pool).await?;
    crate::backend::db::postgres::users::transitions::lock_membership(&mut tx).await?;
    let Some(role) = sqlx::query_scalar::<_, String>(
        "SELECT role FROM organization_invitations WHERE org_id=$1 AND user_id=$2 FOR UPDATE",
    )
    .bind(org)
    .bind(actor)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Missing);
    };
    if !active(&mut tx, actor).await? {
        return Ok(Denied);
    }
    if !remaining_active_owner(&mut tx, org, -1).await? {
        return Ok(Suspended);
    }
    let Ok(role) = role.parse::<OrgRole>() else {
        return Ok(Denied);
    };
    let member = sqlx::query_as(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,$3) RETURNING *",
    )
    .bind(org)
    .bind(actor)
    .bind(role.as_str())
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM organization_invitations WHERE org_id=$1 AND user_id=$2")
        .bind(org)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(member))
}

pub async fn decline_organization_invitation(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<()>> {
    let deleted =
        sqlx::query("DELETE FROM organization_invitations WHERE org_id=$1 AND user_id=$2")
            .bind(org)
            .bind(actor)
            .execute(pool)
            .await?
            .rows_affected();
    Ok(if deleted == 0 { Missing } else { Applied(()) })
}
