use crate::backend::db::organizations::*;
use anyhow::Result;
use sqlx::SqlitePool;
use OrganizationOutcome::{Applied, Conflict, Denied};

use super::authority::*;

pub async fn create_organization(
    pool: &SqlitePool,
    actor: i64,
    input: &NewOrganization,
) -> Result<OrganizationOutcome<OrganizationMetadata>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !active(&mut tx, actor).await? {
        return Ok(Denied);
    }
    // Writer serialization is sufficient only once ALL account insertion paths
    // check organizations in their write transaction. Parent owns those changes.
    let occupied: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE username=?1 UNION ALL SELECT 1 FROM organizations WHERE name=?1)")
        .bind(&input.slug).fetch_one(&mut *tx).await?;
    if occupied {
        return Ok(Conflict);
    }
    let id: Option<i64> = sqlx::query_scalar("INSERT INTO organizations(name,display_name,description) VALUES(?1,?2,?3) ON CONFLICT(name) DO NOTHING RETURNING id")
        .bind(&input.slug).bind(input.profile.display_name.clone().flatten()).bind(input.profile.description.clone().flatten()).fetch_optional(&mut *tx).await?;
    let Some(id) = id else {
        return Ok(Conflict);
    };
    sqlx::query("INSERT INTO org_memberships(org_id,user_id,role) VALUES(?1,?2,'owner')")
        .bind(id)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
    let result = metadata(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Applied(result))
}
pub async fn update_organization_profile(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    input: &OrganizationProfile,
) -> Result<OrganizationOutcome<OrganizationMetadata>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let role = require_org!(tx, actor, org);
    if !administers(role) {
        return Ok(Denied);
    }
    // This transaction holds the write lock, so an omitted field keeps the value committed
    // before this write.
    sqlx::query("UPDATE organizations SET display_name=CASE WHEN ?2 THEN ?3 ELSE display_name END,description=CASE WHEN ?4 THEN ?5 ELSE description END,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1")
        .bind(org)
        .bind(input.display_name.is_some())
        .bind(input.display_name.clone().flatten())
        .bind(input.description.is_some())
        .bind(input.description.clone().flatten())
        .execute(&mut *tx)
        .await?;
    let result = metadata(&mut tx, org).await?;
    tx.commit().await?;
    Ok(Applied(result))
}
pub async fn set_organization_avatar(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    file: Option<uuid::Uuid>,
) -> Result<OrganizationOutcome<Option<uuid::Uuid>>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let role = require_org!(tx, actor, org);
    if !administers(role) {
        return Ok(Denied);
    }
    if let Some(file) = file {
        // Activate verified form uploads in the same transaction as assignment.
        let usable: Option<uuid::Uuid> = sqlx::query_scalar(
            "UPDATE files SET is_active=TRUE,upload_expires_at=NULL,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 AND user_id=?2 AND (is_active OR (confirmed_at IS NOT NULL AND upload_expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now'))) AND reason='org_avatar' AND content_type IN ('image/png','image/jpeg','image/gif','image/webp') RETURNING id",
        )
        .bind(file)
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
        if usable.is_none() {
            return Ok(Conflict);
        }
    }
    let previous: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT avatar_file_id FROM organizations WHERE id=?1")
            .bind(org)
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query("UPDATE organizations SET avatar_file_id=?2,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1")
        .bind(org)
        .bind(file)
        .execute(&mut *tx)
        .await?;
    if let Some(old) = previous.filter(|old| Some(*old) != file) {
        crate::backend::db::sqlite::files::delete_unreferenced_file_in_transaction(&mut tx, old)
            .await?;
    }
    tx.commit().await?;
    Ok(Applied(previous))
}
pub async fn get_organization_avatar(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<Option<uuid::Uuid>>> {
    let mut tx = begin_read(pool).await?;
    require_read_org!(tx, actor, org);
    let avatar = sqlx::query_scalar("SELECT avatar_file_id FROM organizations WHERE id=?1")
        .bind(org)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(avatar))
}
pub async fn get_public_organization(
    pool: &SqlitePool,
    name: &str,
) -> Result<Option<crate::backend::db::models::Organization>> {
    // Governance-suspended means no active owner, as in get_organization_metadata.
    Ok(sqlx::query_as(
        "SELECT o.* FROM organizations o WHERE o.name=?1 AND EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=o.id AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password)",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?)
}
pub async fn get_organization_metadata(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<OrganizationMetadata>> {
    let mut tx = begin_read(pool).await?;
    require_read_org!(tx, actor, org);
    let result = metadata(&mut tx, org).await?;
    tx.commit().await?;
    Ok(Applied(result))
}
pub async fn list_actor_organizations(
    pool: &SqlitePool,
    actor: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<OrganizationMetadata>>> {
    let mut tx = begin_read(pool).await?;
    if !active(&mut tx, actor).await? {
        return Ok(Denied);
    }
    let rows = sqlx::query_as("SELECT o.id,o.name,o.display_name,o.description,o.is_verified,NOT EXISTS(SELECT 1 FROM org_memberships own JOIN users u ON u.id=own.user_id WHERE own.org_id=o.id AND own.role='owner' AND NOT u.is_banned AND NOT u.must_change_password) AS governance_suspended FROM organizations o JOIN org_memberships m ON m.org_id=o.id WHERE m.user_id=?1 AND m.role IN ('owner','admin','member') AND o.id>?2 ORDER BY o.id LIMIT ?3")
        .bind(actor).bind(page.after).bind(page.limit).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
/// Bounded display choices, not authorization for a later write.
pub async fn list_repository_creation_organizations(
    pool: &SqlitePool,
    actor: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<OrganizationMetadata>>> {
    let mut tx = begin_read(pool).await?;
    if !active(&mut tx, actor).await? {
        return Ok(Denied);
    }
    // Role and usable governance filter BEFORE LIMIT in the same read snapshot.
    // Never enumerate memberships or apply a site-administrator exception.
    let rows = sqlx::query_as("SELECT o.id,o.name,o.display_name,o.description,o.is_verified,FALSE AS governance_suspended FROM organizations o JOIN org_memberships m ON m.org_id=o.id WHERE m.user_id=?1 AND m.role IN ('owner','admin') AND o.id>?2 AND EXISTS(SELECT 1 FROM org_memberships own JOIN users u ON u.id=own.user_id WHERE own.org_id=o.id AND own.role='owner' AND NOT u.is_banned AND NOT u.must_change_password) ORDER BY o.id LIMIT ?3")
        .bind(actor).bind(page.after).bind(page.limit).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
