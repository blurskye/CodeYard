use super::authority::*;
use crate::backend::db::organizations::invitations::InvitationOffer;
use crate::backend::db::{
    models::{OrgInvitation, OrgMembership, OrgRole, ReceivedInvitation, Team},
    organizations::*,
};
use anyhow::Result;
use sqlx::{PgConnection, PgPool};
use OrganizationOutcome::{Applied, Conflict, Denied, Missing, Suspended};

pub(super) const RECIPIENT: &str = "((i.email IS NULL AND i.user_id=$1) OR EXISTS(SELECT 1 FROM users u WHERE u.id=$1 AND ((u.email_verified AND lower(u.email)=i.email) OR EXISTS(SELECT 1 FROM user_emails e WHERE e.user_id=u.id AND e.verified AND lower(e.email)=i.email))))";
pub(super) const LIVE: &str = "i.failed_at IS NULL AND i.expires_at>clock_timestamp()";

pub async fn invite_organization_member(
    pool: &PgPool,
    actor: i64,
    org: i64,
    target: i64,
    role: OrgRole,
) -> Result<OrganizationOutcome<OrgInvitation>> {
    save(
        pool,
        actor,
        org,
        &NewInvitation::user(target, role),
        false,
        false,
    )
    .await
}
pub async fn set_organization_invitation_by_owner(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    target: i64,
    role: OrgRole,
) -> Result<OrganizationOutcome<OrgInvitation>> {
    save(
        pool,
        actor,
        org,
        &NewInvitation::user(target, role),
        true,
        true,
    )
    .await
}
pub async fn create_organization_invitation(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    input: &NewInvitation,
    owner_only: bool,
) -> Result<OrganizationOutcome<OrgInvitation>> {
    save(pool, actor, org, input, owner_only, false).await
}
async fn save(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    input: &NewInvitation,
    owner_only: bool,
    replace: bool,
) -> Result<OrganizationOutcome<OrgInvitation>> {
    let mut tx = begin(pool).await?;
    let policy = if replace {
        InvitationOffer::Replace
    } else {
        InvitationOffer::Create
    };
    let result = save_in_transaction(&mut tx, actor, org, input, owner_only, policy).await?;
    if matches!(result, Applied(_)) {
        tx.commit().await?;
    }
    Ok(result)
}
// The caller holds the membership transition lock and owns commit or rollback.
pub(super) async fn save_in_transaction(
    mut tx: &mut PgConnection,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    input: &NewInvitation,
    owner_only: bool,
    policy: InvitationOffer,
) -> Result<OrganizationOutcome<OrgInvitation>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let replace = matches!(policy, InvitationOffer::Replace);
    let target = invitation_target(tx, input).await?;
    let accounts: Vec<i64> = [Some(actor), target].into_iter().flatten().collect();
    crate::backend::db::postgres::users::transitions::lock_accounts_shared(tx, &accounts).await?;
    let actor_role = require_org!(tx, actor, org);
    if !credential(tx, &grant, org, "members").await? {
        return Ok(Denied);
    }
    if !administers(actor_role)
        || (owner_only && actor_role != OrgRole::Owner)
        || (!input.reinstate && !can_change_member(actor_role, None, Some(input.role)))
    {
        return Ok(Denied);
    }
    if !input.valid() {
        return Ok(Conflict);
    }
    if invitation_target(tx, input).await? != target {
        return Ok(Conflict);
    }
    if let Some(target) = target {
        if !active(tx, target).await? || super::blocks::target_blocked(tx, org, target).await? {
            return Ok(Denied);
        }
        if membership_role(tx, org, target).await?.is_some() {
            return Ok(Conflict);
        }
        if matches!(policy, InvitationOffer::Retain) {
            let sql=format!("SELECT i.* FROM organization_invitations i WHERE {RECIPIENT} AND i.org_id=$2 AND {LIVE} ORDER BY i.id LIMIT 1 FOR UPDATE");
            let previous: Option<OrgInvitation> = sqlx::query_as(&sql)
                .bind(target)
                .bind(org)
                .fetch_optional(&mut *tx)
                .await?;
            if let Some(previous) = previous {
                if !previous
                    .role
                    .parse::<OrgRole>()
                    .ok()
                    .is_some_and(|role| can_change_member(actor_role, None, Some(role)))
                {
                    return Ok(Denied);
                }
                return Ok(Applied(previous));
            }
        }
    }
    let role = if input.reinstate {
        let Some(target) = target else {
            return Ok(Conflict);
        };
        let old: Option<String> = sqlx::query_scalar(
            "SELECT role FROM organization_member_history WHERE org_id=$1 AND user_id=$2",
        )
        .bind(org)
        .bind(target)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(role) = old.and_then(|role| role.parse::<OrgRole>().ok()) else {
            return Ok(Conflict);
        };
        role
    } else {
        input.role
    };
    if !can_change_member(actor_role, None, Some(role)) {
        return Ok(Denied);
    }
    let team_ids = match &input.teams {
        None if !replace => Some(Vec::new()),
        None => None,
        Some(InvitationTeams::Ids(ids)) => Some(ids.clone()),
        Some(InvitationTeams::Slugs(names)) => {
            let mut ids = Vec::new();
            for name in names {
                let id: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM teams WHERE org_id=$1 AND slug=$2 FOR KEY SHARE",
                )
                .bind(org)
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
                let Some(id) = id else {
                    return Ok(Conflict);
                };
                ids.push(id);
            }
            Some(ids)
        }
    };
    if let Some(ids) = &team_ids {
        for id in ids {
            let valid: Option<i64> =
                sqlx::query_scalar("SELECT id FROM teams WHERE id=$1 AND org_id=$2 FOR KEY SHARE")
                    .bind(id)
                    .bind(org)
                    .fetch_optional(&mut *tx)
                    .await?;
            if valid.is_none() {
                return Ok(Conflict);
            }
        }
    }
    let recipient = RECIPIENT.replace("$1", "$2");
    let mut previous: Vec<OrgInvitation> = sqlx::query_as(&format!("SELECT i.* FROM organization_invitations i WHERE i.org_id=$1 AND (($2::bigint IS NOT NULL AND {recipient}) OR ($3::text IS NOT NULL AND i.email=$3)) ORDER BY i.id LIMIT 2 FOR UPDATE"))
        .bind(org).bind(target).bind(&input.email).fetch_all(&mut *tx).await?;
    if previous.len() > 1 {
        return Ok(Conflict);
    }
    let invitation: OrgInvitation = if let Some(previous) = previous.pop() {
        if !replace && previous.failed_at.is_none() && previous.expires_at > chrono::Utc::now() {
            return Ok(Conflict);
        }
        sqlx::query("DELETE FROM email_outbox WHERE organization_invitation_id=$1")
            .bind(previous.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query_as("UPDATE organization_invitations SET user_id=$2,email=$3,role=$4,invited_by_id=$5,created_at=NOW(),expires_at=NOW()+INTERVAL '7 days',failed_at=NULL,failed_reason=NULL WHERE id=$1 RETURNING *")
            .bind(previous.id).bind(target).bind(&input.email).bind(role.as_str()).bind(actor).fetch_one(&mut *tx).await?
    } else {
        sqlx::query_as("INSERT INTO organization_invitations(org_id,user_id,email,role,invited_by_id) VALUES($1,$2,$3,$4,$5) RETURNING *")
            .bind(org).bind(target).bind(&input.email).bind(role.as_str()).bind(actor).fetch_one(&mut *tx).await?
    };
    if let Some(ids) = &team_ids {
        sqlx::query("DELETE FROM team_invitations WHERE invitation_id=$1")
            .bind(invitation.id)
            .execute(&mut *tx)
            .await?;
        for id in ids {
            sqlx::query("INSERT INTO team_invitations(invitation_id,team_id,role) SELECT $1,t.id,'member' FROM teams t WHERE t.id=$2 AND t.org_id=$3 ON CONFLICT DO NOTHING").bind(invitation.id).bind(id).bind(org).execute(&mut *tx).await?;
        }
    }
    queue_invitation_email(tx, actor, org, target, &invitation).await?;
    Ok(Applied(invitation))
}

async fn invitation_target(tx: &mut PgConnection, input: &NewInvitation) -> Result<Option<i64>> {
    Ok(if let Some(email) = &input.email {
        sqlx::query_scalar("SELECT u.id FROM users u WHERE (u.email_verified AND lower(u.email)=$1) OR EXISTS(SELECT 1 FROM user_emails e WHERE e.user_id=u.id AND e.verified AND lower(e.email)=$1) LIMIT 1")
            .bind(email).fetch_optional(tx).await?
    } else {
        input.user_id
    })
}

/// Queues the offer's email with the offer, so a saved invitation always has its message. The
/// address is the invited one, or the invitee's notification address. An invitee who blocks
/// the inviter, or whom the inviter blocks, gets none, as they get no notification.
async fn queue_invitation_email(
    tx: &mut PgConnection,
    actor: i64,
    org: i64,
    target: Option<i64>,
    invitation: &OrgInvitation,
) -> Result<()> {
    if let Some(user) = target {
        if crate::backend::db::postgres::users::blocks::interaction_blocked(&mut *tx, user, actor)
            .await?
        {
            return Ok(());
        }
    }
    let to = match (&invitation.email, target) {
        (Some(email), _) => Some(email.clone()),
        (None, Some(user)) => {
            crate::backend::db::postgres::email::notification_address(&mut *tx, user).await?
        }
        (None, None) => None,
    };
    let Some(to) = to else {
        return Ok(());
    };
    let name: String = sqlx::query_scalar("SELECT name FROM organizations WHERE id=$1")
        .bind(org)
        .fetch_one(&mut *tx)
        .await?;
    let mail = crate::backend::email::organization_invitation(&name);
    crate::backend::db::postgres::email::queue_email(
        tx,
        &to,
        &mail.subject,
        &mail.body,
        Some(invitation.id),
    )
    .await
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
    let sql = format!("SELECT i.* FROM organization_invitations i WHERE i.org_id=$1 AND i.id>$2 AND {LIVE} ORDER BY i.id LIMIT $3");
    let rows = sqlx::query_as(&sql)
        .bind(org)
        .bind(page.after)
        .bind(page.limit)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
pub async fn invitation_page(
    pool: &PgPool,
    actor: i64,
    org: i64,
    filter: &InvitationFilter,
    window: (i64, i64),
) -> Result<OrganizationOutcome<Vec<OrgInvitation>>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    if !administers(role) || (filter.owner_only && role != OrgRole::Owner) {
        return Ok(Denied);
    }
    if filter.empty {
        return Ok(Applied(Vec::new()));
    }
    let rows = sqlx::query_as("SELECT id,org_id,user_id,email,role,invited_by_id,created_at,expires_at,CASE WHEN failed_at IS NOT NULL THEN failed_at WHEN expires_at<=NOW() THEN expires_at END AS failed_at,CASE WHEN failed_at IS NOT NULL THEN failed_reason WHEN expires_at<=NOW() THEN 'Invitation expired' END AS failed_reason FROM organization_invitations WHERE org_id=$1 AND (failed_at IS NOT NULL OR expires_at<=NOW())=$2 AND ($3::text IS NULL OR role=$3 OR ($3='member' AND role='admin')) AND id>$6 ORDER BY id LIMIT $4 OFFSET $5")
        .bind(org).bind(filter.failed).bind(filter.role.as_ref().map(OrgRole::as_str)).bind(window.1).bind(window.0).bind(filter.after).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
pub async fn invitation_teams(
    pool: &PgPool,
    actor: i64,
    org: i64,
    id: i64,
    window: (i64, i64),
) -> Result<OrganizationOutcome<Vec<Team>>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM organization_invitations WHERE org_id=$1 AND id=$2)",
    )
    .bind(org)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if !exists {
        return Ok(Missing);
    }
    let rows = sqlx::query_as("SELECT t.* FROM team_invitations i JOIN teams t ON t.id=i.team_id WHERE i.invitation_id=$1 AND t.org_id=$2 ORDER BY t.id LIMIT $3 OFFSET $4").bind(id).bind(org).bind(window.1).bind(window.0).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
pub async fn invitation_team_counts(
    pool: &PgPool,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, i64>> {
    let rows: Vec<(i64,i64)> = sqlx::query_as("SELECT invitation_id,COUNT(*) FROM team_invitations WHERE invitation_id=ANY($1) GROUP BY invitation_id").bind(ids).fetch_all(pool).await?;
    Ok(rows.into_iter().collect())
}

pub async fn cancel_organization_invitation(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<()>> {
    cancel(pool, actor, org, target, false, false).await
}
pub async fn cancel_invitation_id(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    id: i64,
    owner_only: bool,
) -> Result<OrganizationOutcome<()>> {
    cancel(pool, actor, org, id, true, owner_only).await
}
async fn cancel(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    target: i64,
    by_id: bool,
    owner_only: bool,
) -> Result<OrganizationOutcome<()>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    crate::backend::db::postgres::users::transitions::lock_accounts_shared(&mut tx, &[actor])
        .await?;
    let actor_role = require_org!(tx, actor, org);
    if !credential(&mut tx, &grant, org, "members").await?
        || !administers(actor_role)
        || (owner_only && actor_role != OrgRole::Owner)
    {
        return Ok(Denied);
    }
    let outcome = cancel_offers(&mut tx, actor_role, org, target, by_id).await?;
    if matches!(outcome, Applied(())) {
        tx.commit().await?;
    }
    Ok(outcome)
}

/// Deletes the pending offers that match `target` (a recipient account, or an invitation id
/// when `by_id`) inside the caller's transaction, after checking that `actor_role` may cancel
/// each of them.
pub(super) async fn cancel_offers(
    tx: &mut PgConnection,
    actor_role: OrgRole,
    org: i64,
    target: i64,
    by_id: bool,
) -> Result<OrganizationOutcome<()>> {
    // A username also matches a verified email offer; an invitation id is exact.
    let matches = if by_id {
        "i.id=$2".to_owned()
    } else {
        RECIPIENT.replace("$1", "$2")
    };
    // Every offer the recipient holds goes, so none is left to grant membership.
    let sql = format!(
        "SELECT i.* FROM organization_invitations i WHERE i.org_id=$1 AND {matches} ORDER BY i.id FOR UPDATE"
    );
    let invitations: Vec<OrgInvitation> = sqlx::query_as(&sql)
        .bind(org)
        .bind(target)
        .fetch_all(&mut *tx)
        .await?;
    if invitations.is_empty() {
        return Ok(Missing);
    }
    for invited in &invitations {
        let Ok(role) = invited.role.parse::<OrgRole>() else {
            return Ok(Denied);
        };
        if !can_change_member(actor_role, None, Some(role)) {
            return Ok(Denied);
        }
    }
    for invited in &invitations {
        sqlx::query("DELETE FROM email_outbox WHERE organization_invitation_id=$1")
            .bind(invited.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM organization_invitations WHERE id=$1")
            .bind(invited.id)
            .execute(&mut *tx)
            .await?;
    }
    Ok(Applied(()))
}

pub async fn list_received_organization_invitations(
    pool: &PgPool,
    actor: i64,
    page: OrganizationPage,
) -> Result<Vec<ReceivedInvitation>> {
    let sql = format!("SELECT i.id,i.org_id,o.name AS org_name,o.display_name AS org_display_name,i.role,i.created_at FROM organization_invitations i JOIN organizations o ON o.id=i.org_id WHERE {RECIPIENT} AND i.id>$2 AND {LIVE} ORDER BY i.id LIMIT $3");
    Ok(sqlx::query_as(&sql)
        .bind(actor)
        .bind(page.after)
        .bind(page.limit)
        .fetch_all(pool)
        .await?)
}
/// Accepts `invitation`, or the oldest live offer when it is `None`. Every offer from the
/// organization ends once the account is a member.
pub async fn accept_organization_invitation(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    invitation: Option<i64>,
) -> Result<OrganizationOutcome<OrgMembership>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    crate::backend::db::postgres::users::transitions::lock_accounts_shared(&mut tx, &[actor])
        .await?;
    if sqlx::query_scalar::<_, i64>("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(org)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        return Ok(Missing);
    }
    // Hold the account lock before testing verified address ownership. Address removal and
    // primary-address changes lock the same row and cannot revoke proof under acceptance.
    let actor_active = active(&mut tx, actor).await?;
    let sql = format!("SELECT i.id FROM organization_invitations i WHERE {RECIPIENT} AND i.org_id=$2 ORDER BY i.id FOR UPDATE");
    sqlx::query(&sql)
        .bind(actor)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    let sql = format!("SELECT i.* FROM organization_invitations i WHERE {RECIPIENT} AND i.org_id=$2 AND ($3 IS NULL OR i.id=$3) AND {LIVE} AND NOT EXISTS(SELECT 1 FROM organization_member_history h WHERE h.org_id=i.org_id AND h.user_id=$1 AND h.removed_at>=i.created_at) ORDER BY i.id LIMIT 1");
    let Some(invitation): Option<OrgInvitation> = sqlx::query_as(&sql)
        .bind(actor)
        .bind(org)
        .bind(invitation)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(Missing);
    };
    if !actor_active || super::blocks::target_blocked(&mut tx, org, actor).await? {
        return Ok(Denied);
    }
    if !remaining_active_owner(&mut tx, org, -1).await? {
        return Ok(Suspended);
    }
    let Ok(role) = invitation.role.parse::<OrgRole>() else {
        return Ok(Denied);
    };
    if !credential(&mut tx, &grant, org, "members").await? {
        return Ok(Denied);
    }
    if membership_role(&mut tx, org, actor).await?.is_some() {
        return Ok(Conflict);
    }
    let member = sqlx::query_as(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,$3) RETURNING *",
    )
    .bind(org)
    .bind(actor)
    .bind(role.as_str())
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO team_memberships(team_id,user_id,role) SELECT ti.team_id,$1,ti.role FROM team_invitations ti JOIN teams t ON t.id=ti.team_id WHERE ti.invitation_id=$2 AND t.org_id=$3 AND ti.role IN ('member','maintainer') ON CONFLICT(team_id,user_id) DO UPDATE SET role=EXCLUDED.role")
        .bind(actor).bind(invitation.id).bind(org).execute(&mut *tx).await?;
    let sql = format!("DELETE FROM email_outbox WHERE organization_invitation_id IN (SELECT i.id FROM organization_invitations i WHERE {RECIPIENT} AND i.org_id=$2)");
    sqlx::query(&sql)
        .bind(actor)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    let sql =
        format!("DELETE FROM organization_invitations AS i WHERE {RECIPIENT} AND i.org_id=$2");
    sqlx::query(&sql)
        .bind(actor)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(member))
}
/// Declines `invitation`, or every offer from the organization when it is `None`.
pub async fn decline_organization_invitation(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    invitation: Option<i64>,
) -> Result<OrganizationOutcome<()>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    if !super::super::credentials::live(&mut tx, &grant, true).await? {
        return Ok(Denied);
    }
    crate::backend::db::postgres::users::transitions::lock_accounts_shared(&mut tx, &[actor])
        .await?;
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(org)
        .execute(&mut *tx)
        .await?;
    let _ = active(&mut tx, actor).await?;
    let sql = format!("SELECT i.id FROM organization_invitations i WHERE {RECIPIENT} AND i.org_id=$2 ORDER BY i.id FOR UPDATE");
    sqlx::query(&sql)
        .bind(actor)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    let sql = format!("DELETE FROM email_outbox WHERE organization_invitation_id IN (SELECT i.id FROM organization_invitations i WHERE {RECIPIENT} AND i.org_id=$2 AND ($3 IS NULL OR i.id=$3))");
    sqlx::query(&sql)
        .bind(actor)
        .bind(org)
        .bind(invitation)
        .execute(&mut *tx)
        .await?;
    let sql = format!("DELETE FROM organization_invitations AS i WHERE {RECIPIENT} AND i.org_id=$2 AND ($3 IS NULL OR i.id=$3)");
    let deleted = sqlx::query(&sql)
        .bind(actor)
        .bind(org)
        .bind(invitation)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(if deleted == 0 { Missing } else { Applied(()) })
}
