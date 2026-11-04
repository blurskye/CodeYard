//! Native membership commands; last-owner policy belongs to the DAO.

use crate::organizations::organization::unavailable;
use crate::organizations::organization::unit;
use crate::organizations::organization::Fields;
use crate::user::fine_grained::orgs as view;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_notifications::create::organization_invitation;
use std::collections::HashMap;

pub(super) async fn project(
    ctx: &PageContext,
    members: Vec<(i64, String)>,
) -> Result<Vec<view::Member>, Response> {
    // IDs come only from the authorized, paginated organization/team reads.
    let ids = members.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let users: HashMap<_, _> =
        codeyard_db_identity::users::accounts::get_users_by_ids(&ctx.state.pool, &ids)
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

pub(super) async fn project_invitations(
    ctx: &PageContext,
    invitations: Vec<codeyard_db_core::models::OrgInvitation>,
) -> Result<Vec<view::PendingInvitation>, Response> {
    // An email-addressed invitation is accepted by whoever currently owns the address, so
    // the account resolved when it was sent is not its recipient.
    let identities = invitations
        .iter()
        .filter(|invite| invite.email.is_none())
        .filter_map(|invite| invite.user_id.map(|user| (user, invite.role.clone())))
        .collect();
    let users: HashMap<_, _> = project(ctx, identities)
        .await?
        .into_iter()
        .map(|user| (user.user_id, user.username))
        .collect();
    invitations
        .into_iter()
        .map(|invite| {
            let username = match invite.email {
                Some(email) => email,
                None => invite
                    .user_id
                    .and_then(|id| users.get(&id).cloned())
                    .ok_or_else(|| unavailable(ctx))?,
            };
            Ok(view::PendingInvitation {
                id: invite.id,
                user_id: invite.user_id,
                username,
                role: invite.role,
                failed_reason: invite.failed_reason,
            })
        })
        .collect()
}

pub(super) enum Command {
    Visibility {
        public: bool,
    },
    /// Offers membership; the person joins only when they accept.
    Invite {
        username: String,
        role: OrgRole,
        team_slugs: Vec<String>,
        reinstate: bool,
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
    CancelInvitationId {
        invitation: i64,
    },
    Block {
        username: String,
    },
    Unblock {
        user: i64,
    },
    ConvertOutside {
        user: i64,
    },
    RemoveOutside {
        user: i64,
    },
}
impl Command {
    pub(super) fn parse(form: &Fields) -> Result<Self, ()> {
        Ok(
            match codeyard_api_http::shared::user::responses::field(form, "action") {
                "membership-visibility" => Self::Visibility {
                    public: match codeyard_api_http::shared::user::responses::field(
                        form,
                        "visibility",
                    ) {
                        "public" => true,
                        "private" => false,
                        _ => return Err(()),
                    },
                },
                "add-member" => {
                    let recipient =
                        codeyard_api_http::shared::user::responses::field(form, "username");
                    let username = if recipient.contains('@') {
                        recipient.trim().to_ascii_lowercase()
                    } else {
                        crate::organizations::organization::username(form)?
                    };
                    let team_slugs =
                        codeyard_api_http::shared::user::responses::field(form, "team_slugs")
                            .split(',')
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .map(str::to_owned)
                            .collect();
                    let reinstate = match codeyard_api_http::shared::user::responses::field(
                        form,
                        "reinstate",
                    ) {
                        "" => false,
                        "yes" => true,
                        _ => return Err(()),
                    };
                    Self::Invite {
                        username,
                        role: super::parse_organization_role(
                            codeyard_api_http::shared::user::responses::field(form, "role"),
                        )
                        .map_err(|_| ())?,
                        team_slugs,
                        reinstate,
                    }
                }
                "member-role" => Self::ChangeRole {
                    user: crate::organizations::organization::id(form, "user_id")?,
                    role: super::parse_organization_role(
                        codeyard_api_http::shared::user::responses::field(form, "role"),
                    )
                    .map_err(|_| ())?,
                },
                "remove-member" => Self::Remove {
                    user: crate::organizations::organization::id(form, "user_id")?,
                },
                "block-user" => {
                    let username =
                        codeyard_api_http::shared::user::responses::field(form, "username").trim();
                    if username.is_empty() {
                        return Err(());
                    }
                    Self::Block {
                        username: username.to_owned(),
                    }
                }
                "unblock-user" => Self::Unblock {
                    user: crate::organizations::organization::id(form, "user_id")?,
                },
                "convert-outside" => Self::ConvertOutside {
                    user: crate::organizations::organization::id(form, "user_id")?,
                },
                "remove-outside" => Self::RemoveOutside {
                    user: crate::organizations::organization::id(form, "user_id")?,
                },
                "cancel-invitation"
                    if form.contains_key("invitation_id") && !form.contains_key("user_id") =>
                {
                    Self::CancelInvitationId {
                        invitation: crate::organizations::organization::id(form, "invitation_id")?,
                    }
                }
                "cancel-invitation" if !form.contains_key("invitation_id") => {
                    Self::CancelInvitation {
                        user: crate::organizations::organization::id(form, "user_id")?,
                    }
                }
                _ => return Err(()),
            },
        )
    }
}
pub(super) async fn target(ctx: &PageContext, username: &str) -> Result<Option<i64>, Response> {
    codeyard_db_identity::users::accounts::get_user_by_username(&ctx.state.pool, username)
        .await
        .map(|user| user.map(|user| user.id))
        .map_err(|_| unavailable(ctx))
}
pub(super) async fn apply(
    ctx: &PageContext,
    actor: i64,
    org: i64,
    command: Command,
) -> Result<OrganizationOutcome<()>, Response> {
    let pool = &ctx.state.pool;
    let result = match command {
        Command::Visibility { public } => {
            codeyard_db_orgs::organizations::discovery::set_membership_public(
                pool,
                ctx.database_actor(actor),
                org,
                public,
            )
            .await
        }
        Command::Invite {
            username,
            role,
            team_slugs,
            reinstate,
        } => {
            let email = username.contains('@').then(|| username.clone());
            let user = if email.is_some() {
                None
            } else {
                target(ctx, &username).await?
            };
            if user.is_none() && email.is_none() {
                // Only an owner or admin may learn that the name is unknown; anyone else is refused.
                return match super::get_organization_role(pool, actor, org).await {
                    Ok(OrganizationOutcome::Applied(role))
                        if codeyard_db_core::queries::organizations::administers(role) =>
                    {
                        Ok(OrganizationOutcome::Missing)
                    }
                    Ok(_) => Ok(OrganizationOutcome::Denied),
                    Err(_) => Err(unavailable(ctx)),
                };
            }
            let input = codeyard_db_orgs::organizations::invitations::NewInvitation {
                user_id: user,
                email,
                role,
                reinstate,
                teams: Some(codeyard_db_orgs::organizations::InvitationTeams::Slugs(
                    team_slugs,
                )),
            };
            let outcome = super::create_organization_invitation(
                pool,
                ctx.database_actor(actor),
                org,
                &input,
                false,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            if let OrganizationOutcome::Applied(invitation) = &outcome {
                organization_invitation(
                    pool,
                    &ctx.state.notification_broadcaster,
                    actor,
                    invitation,
                )
                .await;
            }
            return Ok(unit(outcome));
        }
        Command::CancelInvitation { user } => {
            super::cancel_organization_invitation(pool, ctx.database_actor(actor), org, user).await
        }
        Command::CancelInvitationId { invitation } => {
            super::cancel_invitation_id(pool, ctx.database_actor(actor), org, invitation, false)
                .await
        }
        Command::ChangeRole { user, role } => {
            super::update_organization_member_role(pool, ctx.database_actor(actor), org, user, role)
                .await
                .map(unit)
        }
        Command::Remove { user } => {
            super::remove_organization_member(pool, ctx.database_actor(actor), org, user).await
        }
        Command::Block { username } => {
            let Some(user) = target(ctx, &username).await? else {
                return Ok(OrganizationOutcome::Missing);
            };
            codeyard_db_orgs::organizations::blocks::change(
                pool,
                ctx.database_actor(actor),
                org,
                user,
                true,
            )
            .await
        }
        Command::Unblock { user } => {
            codeyard_db_orgs::organizations::blocks::change(
                pool,
                ctx.database_actor(actor),
                org,
                user,
                false,
            )
            .await
        }
        Command::ConvertOutside { user } => {
            super::convert_to_outside_collaborator(pool, ctx.database_actor(actor), org, user).await
        }
        Command::RemoveOutside { user } => {
            codeyard_db_orgs::organizations::outside::remove(
                pool,
                ctx.database_actor(actor),
                org,
                user,
            )
            .await
        }
    };
    result.map_err(|_| unavailable(ctx))
}
