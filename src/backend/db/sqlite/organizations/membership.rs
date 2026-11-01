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
    if !active(&mut tx, target).await?
        || super::blocks::target_blocked(&mut tx, org, target).await?
    {
        return Ok(Denied);
    }
    if membership_role(&mut tx, org, target).await?.is_some() {
        return Ok(Conflict);
    }
    // A direct addition replaces any pending invitation.
    sqlx::query("DELETE FROM organization_invitations WHERE org_id=?1 AND user_id=?2")
        .bind(org)
        .bind(target)
        .execute(&mut *tx)
        .await?;
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
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    target: i64,
    role: OrgRole,
) -> Result<OrganizationOutcome<OrgMembership>> {
    update_member_role(pool, actor, org, target, role, false).await
}

pub async fn update_organization_member_role_by_owner(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    target: i64,
    role: OrgRole,
) -> Result<OrganizationOutcome<OrgMembership>> {
    update_member_role(pool, actor, org, target, role, true).await
}

async fn update_member_role(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    target: i64,
    role: OrgRole,
    owner_only: bool,
) -> Result<OrganizationOutcome<OrgMembership>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let actor_role = require_org!(tx, actor, org);
    if !credential(&mut tx, &grant, org, "members").await?
        || !administers(actor_role)
        || (owner_only && actor_role != OrgRole::Owner)
    {
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
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<()>> {
    remove_member(pool, actor, org, target, false, Removal::Remove).await
}

/// REST removal: ends the membership, or cancels the pending invitations when the account is
/// not a member, in one transaction so an acceptance cannot slip between the two.
pub async fn remove_member_or_invitation_by_owner(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<()>> {
    remove_member(pool, actor, org, target, true, Removal::RemoveOrCancel).await
}

pub async fn convert_to_outside_collaborator(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<()>> {
    remove_member(pool, actor, org, target, true, Removal::Convert).await
}

#[derive(PartialEq)]
enum Removal {
    Remove,
    RemoveOrCancel,
    Convert,
}

async fn remove_member(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    target: i64,
    owner_only: bool,
    mode: Removal,
) -> Result<OrganizationOutcome<()>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let actor_role = require_org!(tx, actor, org);
    if !credential(&mut tx, &grant, org, "members").await? {
        return Ok(Denied);
    }
    // Any member may leave on their own; the last-owner check below still applies.
    let leaving = actor == target && mode == Removal::Remove && !owner_only;
    if !leaving && (!administers(actor_role) || (owner_only && actor_role != OrgRole::Owner)) {
        return Ok(Denied);
    }
    let Some(old) = membership_role(&mut tx, org, target).await? else {
        return Ok(match mode {
            Removal::Remove => Missing,
            Removal::RemoveOrCancel => {
                let outcome =
                    super::invitations::cancel_offers(&mut tx, actor_role, org, target, false)
                        .await?;
                if matches!(outcome, Applied(())) {
                    tx.commit().await?;
                }
                outcome
            }
            Removal::Convert => Denied,
        });
    };
    // A role this build does not recognize is refused; it cannot be removed here.
    let Ok(old) = old.parse::<OrgRole>() else {
        return Ok(Denied);
    };
    if !leaving && !can_change_member(actor_role, Some(old), None) {
        return Ok(Denied);
    }
    if old == OrgRole::Owner && !remaining_active_owner(&mut tx, org, target).await? {
        return Ok(Conflict);
    }
    if mode == Removal::Convert {
        sqlx::query("INSERT INTO repo_access(user_id,repo_id,access_level) SELECT ?1,r.id,MAX(role.base_level) FROM repositories r JOIN organization_role_assignments a ON a.org_id=r.owner_org_id AND a.user_id IS NULL JOIN organization_roles role ON role.id=a.role_id AND role.base_level BETWEEN 1 AND 5 JOIN teams t ON t.id=a.team_id AND t.org_id=a.org_id AND t.privacy IN ('visible','secret') JOIN effective_team_memberships m ON m.team_id=t.id AND m.user_id=?1 AND m.role IN ('member','maintainer') WHERE r.owner_kind='org' AND r.owner_org_id=?2 AND r.owner_user_id IS NULL GROUP BY r.id ON CONFLICT(user_id,repo_id) DO UPDATE SET access_level=CASE WHEN repo_access.access_level BETWEEN 1 AND 5 THEN MAX(repo_access.access_level,excluded.access_level) ELSE excluded.access_level END").bind(target).bind(org).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO repo_access(user_id,repo_id,access_level) SELECT ?1,a.repo_id,MAX(a.permission) FROM team_repo_access a JOIN teams t ON t.id=a.team_id JOIN effective_team_memberships m ON m.team_id=t.id JOIN repositories r ON r.id=a.repo_id WHERE m.user_id=?1 AND m.role IN ('member','maintainer') AND t.org_id=?2 AND t.privacy IN ('visible','secret') AND r.owner_kind='org' AND r.owner_org_id=?2 AND r.owner_user_id IS NULL AND a.permission BETWEEN 1 AND 5 GROUP BY a.repo_id ON CONFLICT(user_id,repo_id) DO UPDATE SET access_level=CASE WHEN repo_access.access_level BETWEEN 1 AND 5 THEN MAX(repo_access.access_level,excluded.access_level) ELSE excluded.access_level END")
            .bind(target).bind(org).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO organization_member_history(org_id,user_id,role) VALUES(?1,?2,?3) ON CONFLICT(org_id,user_id) DO UPDATE SET role=excluded.role,removed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
        .bind(org).bind(target).bind(old.as_str()).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM team_memberships WHERE user_id=?1 AND team_id IN (SELECT id FROM teams WHERE org_id=?2)")
        .bind(target).bind(org).execute(&mut *tx).await?;
    if matches!(mode, Removal::Remove | Removal::RemoveOrCancel) {
        sqlx::query("DELETE FROM repo_access WHERE user_id=?1 AND repo_id IN (SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=?2)")
            .bind(target).bind(org).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE repository_invitations SET state='cancelled' WHERE invitee_id=?1 AND state='pending' AND repo_id IN (SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=?2)")
        .bind(target).bind(org).execute(&mut *tx).await?;
    let recipient = super::invitations::RECIPIENT;
    sqlx::query(&format!(
        "DELETE FROM organization_invitations AS i WHERE {recipient} AND i.org_id=?2"
    ))
    .bind(target)
    .bind(org)
    .execute(&mut *tx)
    .await?;
    cancel_actor(&mut tx, org, target).await?;
    sqlx::query("DELETE FROM org_memberships WHERE org_id=?1 AND user_id=?2")
        .bind(org)
        .bind(target)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(()))
}
