use crate::backend::db::{models::OrgMembership, organizations::*};
use anyhow::Result;
use sqlx::SqlitePool;
use OrganizationOutcome::{Applied, Conflict, Denied, Missing};

use super::authority::*;

pub async fn recover_suspended_organization_owner(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<OrgMembership>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if sqlx::query_scalar::<_, i64>("SELECT id FROM organizations WHERE id=?1")
        .bind(org)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        return Ok(Missing);
    }
    let admin: bool = sqlx::query_scalar(
        "SELECT is_admin AND NOT is_banned AND NOT must_change_password FROM users WHERE id=?1",
    )
    .bind(actor)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if !admin {
        return Ok(Denied);
    }
    if remaining_active_owner(&mut tx, org, -1).await? {
        return Ok(Conflict);
    }
    if !active(&mut tx, target).await? {
        return Ok(Denied);
    }
    let Some(old) = membership_role(&mut tx, org, target).await? else {
        return Ok(Denied);
    };
    if !matches!(old.as_str(), "admin" | "member") {
        return Ok(Denied);
    }
    let member: OrgMembership = sqlx::query_as(
        "UPDATE org_memberships SET role='owner' WHERE org_id=?1 AND user_id=?2 RETURNING *",
    )
    .bind(org)
    .bind(target)
    .fetch_one(&mut *tx)
    .await?;
    let audit = serde_json::json!({"reason":"no_usable_owner","previous_role":old,"replacement_user_id":target});
    sqlx::query("INSERT INTO audit_log(org_id,actor_id,action,target_type,target_id,metadata) VALUES(?1,?2,'org.governance.recover_owner','org_membership',?3,?4)")
        .bind(org).bind(actor).bind(member.id).bind(audit.to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(member))
}

pub async fn list_ownerless_organizations(
    pool: &SqlitePool,
    actor: i64,
) -> Result<OrganizationOutcome<Vec<OrganizationMetadata>>> {
    let mut tx = begin_read(pool).await?;
    if !site_administrator(&mut tx, actor).await? {
        return Ok(Denied);
    }
    let rows = sqlx::query_as("SELECT o.id,o.name,o.display_name,o.description,o.is_verified,1 AS governance_suspended FROM organizations o WHERE NOT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=o.id AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password) ORDER BY o.name")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
