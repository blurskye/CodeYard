use crate::backend::db::{models::OrgMembership, organizations::*};
use anyhow::Result;
use sqlx::PgPool;
use OrganizationOutcome::{Applied, Conflict, Denied, Missing};

use super::authority::*;

pub async fn recover_suspended_organization_owner(
    pool: &PgPool,
    actor: i64,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<OrgMembership>> {
    let mut tx = begin(pool).await?;
    crate::backend::db::postgres::users::transitions::lock_membership(&mut tx).await?;
    if sqlx::query_scalar::<_, i64>("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(org)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        return Ok(Missing);
    }
    // Lock the users in id order so concurrent recoveries cannot deadlock.
    let users: Vec<(i64, bool, bool)> = sqlx::query_as("SELECT id,is_admin,NOT is_banned AND NOT must_change_password FROM users WHERE id=ANY($1) ORDER BY id FOR SHARE")
        .bind(vec![actor, target]).fetch_all(&mut *tx).await?;
    if !users
        .iter()
        .any(|&(id, admin, active)| id == actor && admin && active)
    {
        return Ok(Denied);
    }
    if remaining_active_owner(&mut tx, org, -1).await? {
        return Ok(Conflict);
    }
    if !users.iter().any(|&(id, _, active)| id == target && active) {
        return Ok(Denied);
    }
    let Some(old) = membership_role(&mut tx, org, target).await? else {
        return Ok(Denied);
    };
    if !matches!(old.as_str(), "admin" | "member") {
        return Ok(Denied);
    }
    let member: OrgMembership = sqlx::query_as(
        "UPDATE org_memberships SET role='owner' WHERE org_id=$1 AND user_id=$2 RETURNING *",
    )
    .bind(org)
    .bind(target)
    .fetch_one(&mut *tx)
    .await?;
    let audit = serde_json::json!({"reason":"no_usable_owner","previous_role":old,"replacement_user_id":target});
    sqlx::query("INSERT INTO audit_log(org_id,actor_id,action,target_type,target_id,metadata) VALUES($1,$2,'org.governance.recover_owner','org_membership',$3,$4)")
        .bind(org).bind(actor).bind(member.id).bind(audit).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(member))
}

pub async fn list_ownerless_organizations(
    pool: &PgPool,
    actor: i64,
) -> Result<OrganizationOutcome<Vec<OrganizationMetadata>>> {
    let mut tx = begin_read(pool).await?;
    if !site_administrator(&mut tx, actor).await? {
        return Ok(Denied);
    }
    let rows = sqlx::query_as("SELECT o.id,o.name,o.display_name,o.description,o.is_verified,TRUE AS governance_suspended FROM organizations o WHERE NOT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=o.id AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password) ORDER BY o.name")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
