use crate::backend::db::organizations::*;
use anyhow::Result;
use codeyard_db_core::models::OrgMembership;
use sqlx::SqlitePool;
use OrganizationOutcome::{Applied, Conflict, Denied, Missing};

use crate::backend::db::sqlite::organizations::authority::*;

pub async fn recover_suspended_organization_owner(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::users::transitions::AdministratorActor>,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<OrgMembership>> {
    let actor = actor.into();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if sqlx::query_scalar::<_, i64>("SELECT id FROM organizations WHERE id=?1")
        .bind(org)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        return Ok(Missing);
    }
    if !crate::backend::db::sqlite::users::transitions::active_admin_session(&mut tx, &actor)
        .await?
    {
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
        .bind(org).bind(actor.user_id).bind(member.id).bind(audit.to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(member))
}

pub async fn list_ownerless_organizations(
    pool: &SqlitePool,
    actor: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<OrganizationMetadata>>> {
    let mut tx = begin_read(pool).await?;
    if !site_administrator(&mut tx, actor).await? {
        return Ok(Denied);
    }
    let rows = sqlx::query_as("SELECT o.id,o.name,NULL AS display_name,NULL AS description,0 AS is_verified,1 AS governance_suspended,NULL AS avatar_file_id FROM organizations o WHERE NOT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=o.id AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password) AND o.id>?1 ORDER BY o.id LIMIT ?2")
        .bind(page.after).bind(page.limit)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
