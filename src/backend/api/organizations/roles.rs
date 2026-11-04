use crate::backend::api::http::context::PageContext;
use crate::backend::api::organizations::organization::Fields;

use crate::backend::db::organizations::roles::RoleTarget;
use crate::backend::db::organizations::OrganizationOutcome;
use axum::{http::StatusCode, response::Response};
use codeyard_webui::pages::orgs as view;

pub(super) struct Command {
    target: RoleTarget,
    role: Option<i64>,
    assigned: bool,
}
impl Command {
    pub(super) fn parse(form: &Fields) -> Result<Self, &'static str> {
        let action = crate::backend::api::organizations::organization::field(form, "action");
        let assigned = action.starts_with("assign-");
        if !assigned
            && crate::backend::api::organizations::organization::field(form, "confirm") != "true"
        {
            return Err("Confirm removing the directly assigned organization roles.");
        }
        let target = if matches!(
            action,
            "assign-user-role" | "revoke-user-role" | "revoke-all-user-roles"
        ) {
            RoleTarget::User(
                crate::backend::api::organizations::organization::username(form)
                    .map_err(|_| "Enter an existing account username.")?,
            )
        } else if matches!(action, "revoke-team-role" | "revoke-all-team-roles") {
            RoleTarget::TeamId(
                crate::backend::api::organizations::organization::id(form, "team_id")
                    .map_err(|_| "Choose an assigned team.")?,
            )
        } else if action == "assign-team-role" {
            let slug =
                crate::backend::api::organizations::organization::field(form, "team_slug").trim();
            // Team handles allow 255 characters in up to 1024 bytes.
            if slug.is_empty() || slug.len() > 1024 || slug.chars().count() > 255 {
                return Err("Enter an existing team handle.");
            }
            RoleTarget::Team(slug.to_owned())
        } else {
            return Err("Invalid organization role operation.");
        };
        let role = if action.starts_with("revoke-all-") {
            None
        } else {
            Some(
                crate::backend::api::organizations::organization::id(form, "role_id")
                    .map_err(|_| "Choose an organization role.")?,
            )
        };
        Ok(Self {
            target,
            role,
            assigned,
        })
    }
}
fn project<T>(
    ctx: &PageContext,
    result: anyhow::Result<OrganizationOutcome<T>>,
) -> Result<T, Response> {
    match result {
        Ok(OrganizationOutcome::Applied(value)) => Ok(value),
        Ok(OrganizationOutcome::Missing) => Err(
            crate::backend::api::organizations::organization::not_found(ctx),
        ),
        Ok(OrganizationOutcome::Denied) => {
            Err(ctx.error(StatusCode::FORBIDDEN, "Organization role access denied."))
        }
        _ => Err(crate::backend::api::organizations::organization::unavailable(ctx)),
    }
}
fn team_view(team: codeyard_db_core::models::Team) -> view::RoleTeam {
    view::RoleTeam {
        id: team.id,
        name: team.name,
        slug: team.slug,
    }
}
pub(super) async fn panel(
    ctx: &PageContext,
    actor: i64,
    org: i64,
) -> Result<view::RolePanel, Response> {
    let definitions = project(
        ctx,
        crate::backend::db::organizations::roles::catalog(&ctx.state.pool, actor, org).await,
    )?;
    let selected = if let Some(value) = ctx.query.get("role") {
        value
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| crate::backend::api::organizations::organization::invalid(ctx))?
    } else {
        definitions
            .first()
            .map(|role| role.id)
            .ok_or_else(|| crate::backend::api::organizations::organization::unavailable(ctx))?
    };
    if !definitions.iter().any(|role| role.id == selected) {
        return Err(crate::backend::api::organizations::organization::not_found(
            ctx,
        ));
    }
    let (user_after, _) =
        crate::backend::api::organizations::organization::cursor(ctx, "role_users_after")
            .ok_or_else(|| crate::backend::api::organizations::organization::invalid(ctx))?;
    let (team_after, _) =
        crate::backend::api::organizations::organization::cursor(ctx, "role_teams_after")
            .ok_or_else(|| crate::backend::api::organizations::organization::invalid(ctx))?;
    let limit = crate::backend::api::organizations::organization::PAGE_LIMIT;
    let mut users = project(
        ctx,
        crate::backend::db::organizations::roles::users(
            &ctx.state.pool,
            actor,
            org,
            selected,
            user_after,
            (0, limit + 1),
        )
        .await,
    )?;
    let more_users = users.len() as i64 > limit;
    users.truncate(limit as usize);
    let mut teams = project(
        ctx,
        crate::backend::db::organizations::roles::teams(
            &ctx.state.pool,
            actor,
            org,
            selected,
            team_after,
            (0, limit + 1),
        )
        .await,
    )?;
    let more_teams = teams.len() as i64 > limit;
    teams.truncate(limit as usize);
    let base = format!("/settings/organizations/{org}?tab=roles&role={selected}");
    let users_next = if more_users {
        users.last().map(|user| {
            format!(
                "{base}&role_users_after={}&role_teams_after={team_after}",
                user.user.id
            )
        })
    } else {
        None
    };
    let teams_next = if more_teams {
        teams.last().map(|team| {
            format!(
                "{base}&role_users_after={user_after}&role_teams_after={}",
                team.id
            )
        })
    } else {
        None
    };
    Ok(view::RolePanel {
        definitions: definitions
            .into_iter()
            .map(|role| view::RoleDefinition {
                id: role.id,
                name: role.name,
                description: role.description,
            })
            .collect(),
        selected,
        users: users
            .into_iter()
            .map(|user| view::RoleUser {
                id: user.user.id,
                username: user.user.username,
                direct: user.direct,
                teams: user.inherited_from.into_iter().map(team_view).collect(),
            })
            .collect(),
        teams: teams.into_iter().map(team_view).collect(),
        users_next,
        teams_next,
    })
}
pub(super) async fn apply(
    ctx: &PageContext,
    actor: i64,
    org: i64,
    command: Command,
) -> anyhow::Result<OrganizationOutcome<()>> {
    crate::backend::db::organizations::roles::change(
        &ctx.state.pool,
        ctx.database_actor(actor),
        org,
        &command.target,
        command.role,
        command.assigned,
    )
    .await
}
