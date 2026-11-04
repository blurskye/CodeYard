use super::authority::*;
use super::invitations::{LIVE, RECIPIENT};
use crate::backend::db::organizations::invitations::InvitationOffer;
use crate::backend::db::organizations::*;
use anyhow::Result;
use codeyard_db_core::models::OrgInvitation;
use codeyard_db_core::models::OrgRole;
use sqlx::SqlitePool;
use OrganizationOutcome::{Applied, Denied, Missing};

pub async fn invite_team_member(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    team: i64,
    target: i64,
    role: TeamRole,
) -> Result<OrganizationOutcome<OrgInvitation>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let actor_role = require_org!(tx, actor, org);
    if !administers(actor_role) {
        return Ok(Denied);
    }
    if scoped_team(&mut tx, org, team).await?.is_none() {
        return Ok(Missing);
    }
    let outcome = super::invitations::save_in_transaction(
        &mut tx,
        grant,
        org,
        &NewInvitation::user(target, OrgRole::Member),
        false,
        InvitationOffer::Retain,
    )
    .await?;
    let Applied(invitation) = outcome else {
        return Ok(outcome);
    };
    sqlx::query("INSERT INTO team_invitations(team_id,invitation_id,role) VALUES(?1,?2,?3) ON CONFLICT(team_id,invitation_id) DO UPDATE SET role=EXCLUDED.role")
        .bind(team).bind(invitation.id).bind(role.as_str()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(invitation))
}

pub async fn pending_team_membership(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    target: i64,
) -> Result<Option<String>> {
    let mut tx = begin_read(pool).await?;
    let Applied(role) = read_authority(&mut tx, actor, org).await? else {
        return Ok(None);
    };
    let Some(selected) = scoped_team(&mut tx, org, team).await? else {
        return Ok(None);
    };
    if !team_visible(&mut tx, actor, role, &selected).await? {
        return Ok(None);
    }
    let recipient = RECIPIENT.replace("?1", "?3");
    let sql=format!("SELECT ti.role FROM team_invitations ti JOIN organization_invitations i ON i.id=ti.invitation_id JOIN users u ON u.id=?3 WHERE ti.team_id=?1 AND i.org_id=?2 AND {recipient} AND {LIVE} AND NOT u.is_banned AND NOT u.must_change_password AND NOT EXISTS(SELECT 1 FROM organization_blocks b WHERE b.org_id=i.org_id AND b.user_id=u.id) AND ti.role IN ('member','maintainer') AND i.role IN ('member','admin','owner') ORDER BY i.id LIMIT 1");
    let role = sqlx::query_scalar(&sql)
        .bind(team)
        .bind(org)
        .bind(target)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(role)
}

pub async fn list_team_invitations(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<TeamInvitation>>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    if !administers(role) {
        return Ok(Denied);
    }
    if scoped_team(&mut tx, org, team).await?.is_none() {
        return Ok(Missing);
    }
    let sql=format!("SELECT i.*,(SELECT COUNT(*) FROM team_invitations x JOIN teams t ON t.id=x.team_id WHERE x.invitation_id=i.id AND t.org_id=i.org_id) AS team_count FROM organization_invitations i JOIN team_invitations ti ON ti.invitation_id=i.id WHERE ti.team_id=?1 AND i.org_id=?2 AND i.id>?3 AND {LIVE} ORDER BY i.id LIMIT ?4");
    let rows = sqlx::query_as(&sql)
        .bind(team)
        .bind(org)
        .bind(page.after)
        .bind(page.limit)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
