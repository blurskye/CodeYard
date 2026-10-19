//! Native membership commands; last-owner policy belongs to the DAO.

use super::organization;
use super::organization::{unavailable, unit, Fields};
use crate::backend::api::http::context::PageContext;
use crate::backend::db;
use crate::backend::db::models::{OrgInvitation, OrgRole};
use crate::backend::db::organizations;
use crate::backend::db::organizations::OrganizationOutcome;
use crate::backend::notifications::create::{
    create_and_broadcast_notification, NotificationRequest,
};
use crate::webui::pages::orgs as view;
use axum::response::Response;
use std::collections::HashMap;

pub(super) async fn project(
    ctx: &PageContext,
    members: Vec<(i64, String)>,
) -> Result<Vec<view::Member>, Response> {
    // IDs come only from the authorized, paginated organization/team reads.
    let ids = members.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let users: HashMap<_, _> = db::users::accounts::get_users_by_ids(&ctx.state.pool, &ids)
        .await
        .map_err(|_| unavailable(ctx))?
        .into_iter()
        .map(|user| (user.id, user.username))
        .collect();
    members
        .into_iter()
        .map(|(user_id, role)| {
            users.get(&user_id).cloned().map(|username| view::Member {
                user_id,
                username,
                role,
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| unavailable(ctx))
}

pub(super) enum Command {
    /// Offers membership; the person joins only when they accept.
    Invite {
        username: String,
        role: OrgRole,
    },
    ChangeRole {
        user: i64,
        role: OrgRole,
    },
    Remove {
        user: i64,
    },
    CancelInvitation {
        user: i64,
    },
}
impl Command {
    pub(super) fn parse(form: &Fields) -> Result<Self, ()> {
        Ok(match organization::field(form, "action") {
            "add-member" => Self::Invite {
                username: organization::username(form)?,
                role: organizations::parse_organization_role(organization::field(form, "role"))
                    .map_err(|_| ())?,
            },
            "member-role" => Self::ChangeRole {
                user: organization::id(form, "user_id")?,
                role: organizations::parse_organization_role(organization::field(form, "role"))
                    .map_err(|_| ())?,
            },
            "remove-member" => Self::Remove {
                user: organization::id(form, "user_id")?,
            },
            "cancel-invitation" => Self::CancelInvitation {
                user: organization::id(form, "user_id")?,
            },
            _ => return Err(()),
        })
    }
}
pub(super) async fn target(ctx: &PageContext, username: &str) -> Result<Option<i64>, Response> {
    db::users::accounts::get_user_by_username(&ctx.state.pool, username)
        .await
        .map(|user| user.map(|user| user.id))
        .map_err(|_| unavailable(ctx))
}
/// Tells the invitee. The invitation stands even when the notification cannot be stored.
async fn notify_invitee(ctx: &PageContext, actor: i64, org: i64, invitation: &OrgInvitation) {
    let name = match organizations::get_organization_metadata(&ctx.state.pool, actor, org).await {
        Ok(OrganizationOutcome::Applied(metadata)) => metadata.name,
        _ => return,
    };
    let request = NotificationRequest {
        user_id: invitation.user_id,
        actor_id: Some(actor),
        repo_id: None,
        kind: "organization_invitation",
        title: &format!("Invitation to join {name}"),
        body: None,
        link: Some("/settings/organizations"),
        metadata: serde_json::json!({ "org_id": org, "role": invitation.role }),
    };
    if let Err(error) = create_and_broadcast_notification(
        &ctx.state.pool,
        &ctx.state.notification_broadcaster,
        request,
    )
    .await
    {
        tracing::warn!(%error, "organization invitation notification was not stored");
    }
}

pub(super) async fn apply(
    ctx: &PageContext,
    actor: i64,
    org: i64,
    command: Command,
) -> Result<OrganizationOutcome<()>, Response> {
    let pool = &ctx.state.pool;
    let result = match command {
        Command::Invite { username, role } => {
            let Some(user) = target(ctx, &username).await? else {
                // Only an owner or admin may learn that the name is unknown; anyone else is refused.
                return match organizations::get_organization_role(pool, actor, org).await {
                    Ok(OrganizationOutcome::Applied(role)) if organizations::administers(role) => {
                        Ok(OrganizationOutcome::Missing)
                    }
                    Ok(_) => Ok(OrganizationOutcome::Denied),
                    Err(_) => Err(unavailable(ctx)),
                };
            };
            let outcome = organizations::invite_organization_member(pool, actor, org, user, role)
                .await
                .map_err(|_| unavailable(ctx))?;
            if let OrganizationOutcome::Applied(invitation) = &outcome {
                notify_invitee(ctx, actor, org, invitation).await;
            }
            return Ok(unit(outcome));
        }
        Command::CancelInvitation { user } => {
            organizations::cancel_organization_invitation(pool, actor, org, user).await
        }
        Command::ChangeRole { user, role } => {
            organizations::update_organization_member_role(pool, actor, org, user, role)
                .await
                .map(unit)
        }
        Command::Remove { user } => {
            organizations::remove_organization_member(pool, actor, org, user).await
        }
    };
    result.map_err(|_| unavailable(ctx))
}
