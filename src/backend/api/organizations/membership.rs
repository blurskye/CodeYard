//! Native membership commands; hierarchy and last-owner policy belong to the DAO.

use super::organization;
use super::organization::{unavailable, unit, Fields};
use crate::backend::api::context::PageContext;
use crate::backend::db;
use crate::backend::db::models::OrgRole;
use crate::backend::db::organizations;
use crate::backend::db::organizations::OrganizationOutcome;
use crate::webui::pages::organizations as view;
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
    Add { username: String, role: OrgRole },
    ChangeRole { user: i64, role: OrgRole },
    Remove { user: i64 },
}
impl Command {
    pub(super) fn parse(form: &Fields) -> Result<Self, ()> {
        Ok(match organization::field(form, "action") {
            "add-member" => Self::Add {
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
pub(super) async fn apply(
    ctx: &PageContext,
    actor: i64,
    org: i64,
    command: Command,
) -> Result<OrganizationOutcome<()>, Response> {
    let pool = &ctx.state.pool;
    let result = match command {
        Command::Add { username, role } => {
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
            organizations::add_organization_member(pool, actor, org, user, role)
                .await
                .map(unit)
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
