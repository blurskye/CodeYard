//! Team ingress and authorized detail projections; DAO policy remains authoritative.

use super::organization;
use super::organization::{invalid, not_found, unavailable, Fields, INVALID_FORM};
use crate::backend::api::context::PageContext;
use crate::backend::app::AppState;
use crate::backend::db::organizations;
use crate::backend::db::organizations::*;
use crate::webui::app::Page;
use crate::webui::pages::organizations as view;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::sync::Arc;

// Refusals that name the rule a field broke. Other malformed input gets INVALID_FORM.
const TEAM_NAME_RULE: &str =
    "Team name must be 3-255 characters: letters, numbers, hyphens or underscores.";
const DISPLAY_NAME_RULE: &str =
    "Display name must be at most 255 characters and cannot contain control characters.";
const EDIT_NAME_RULE: &str =
    "Team name must not be blank, must be at most 255 characters and cannot contain control characters.";
const PRIVACY_RULE: &str = "Privacy must be visible or secret.";
const ROLE_RULE: &str = "Role must be member or maintainer.";
const ACCESS_RULE: &str = "Access must be read, triage, write, maintain or admin.";
const REPOSITORY_RULE: &str = "Choose a repository to grant.";

pub(super) enum Command {
    Create(NewTeam),
    Edit {
        team: i64,
        profile: TeamProfile,
    },
    Delete {
        team: i64,
    },
    AddMember {
        team: i64,
        username: String,
        role: TeamRole,
    },
    RemoveMember {
        team: i64,
        user: i64,
    },
    Grant {
        team: i64,
        repo: i64,
        permission: TeamRepositoryPermission,
    },
    Revoke {
        team: i64,
        repo: i64,
    },
}
impl Command {
    pub(super) fn parse(form: &Fields) -> Result<Self, &'static str> {
        let team = || organization::id(form, "team_id").map_err(|()| INVALID_FORM);
        let privacy =
            || TeamPrivacy::parse(organization::field(form, "privacy")).map_err(|_| PRIVACY_RULE);
        // The create form's Team name is the slug, so each form says which name rule applies.
        let profile = |name: &str, name_rule: &'static str| -> Result<TeamProfile, &'static str> {
            TeamProfile::new(
                name,
                organization::optional(form, "description"),
                privacy()?,
            )
            .map_err(|error| match error {
                OrganizationInputError::Name => name_rule,
                _ => INVALID_FORM,
            })
        };
        Ok(match organization::field(form, "action") {
            "team" => {
                let slug = organization::field(form, "slug");
                // A blank display name shows the team name, so a bad team name is reported as one.
                let display =
                    organization::optional(form, "name").filter(|name| !name.trim().is_empty());
                let (name, name_rule) = match display {
                    Some(name) => (name, DISPLAY_NAME_RULE),
                    None => (slug, TEAM_NAME_RULE),
                };
                Self::Create(
                    NewTeam::new(slug, profile(name, name_rule)?).map_err(|error| match error {
                        OrganizationInputError::Slug => TEAM_NAME_RULE,
                        _ => INVALID_FORM,
                    })?,
                )
            }
            "team-edit" => Self::Edit {
                team: team()?,
                profile: profile(organization::field(form, "name"), EDIT_NAME_RULE)?,
            },
            "team-delete" => Self::Delete { team: team()? },
            "team-member" => Self::AddMember {
                team: team()?,
                username: organization::username(form).map_err(|()| INVALID_FORM)?,
                role: TeamRole::parse(organization::field(form, "role")).map_err(|_| ROLE_RULE)?,
            },
            "remove-team-member" => Self::RemoveMember {
                team: team()?,
                user: organization::id(form, "user_id").map_err(|()| INVALID_FORM)?,
            },
            "grant" => Self::Grant {
                team: team()?,
                repo: organization::id(form, "repo_id").map_err(|()| REPOSITORY_RULE)?,
                permission: TeamRepositoryPermission::new(
                    organization::field(form, "permission")
                        .parse()
                        .map_err(|_| ACCESS_RULE)?,
                )
                .map_err(|_| ACCESS_RULE)?,
            },
            "revoke" => Self::Revoke {
                team: team()?,
                repo: organization::id(form, "repo_id").map_err(|()| INVALID_FORM)?,
            },
            _ => return Err(INVALID_FORM),
        })
    }
}

pub(super) async fn apply(
    ctx: &PageContext,
    actor: i64,
    org: i64,
    command: Command,
) -> Result<OrganizationOutcome<()>, Response> {
    use super::{membership::target, organization::unit};
    let pool = &ctx.state.pool;
    let result = match command {
        Command::Create(input) => organizations::create_organization_team(pool, actor, org, &input)
            .await
            .map(unit),
        Command::Edit { team, profile } => {
            organizations::update_organization_team(pool, actor, org, team, &profile)
                .await
                .map(unit)
        }
        Command::Delete { team } => {
            organizations::delete_organization_team(pool, actor, org, team).await
        }
        Command::AddMember {
            team,
            username,
            role,
        } => {
            let Some(user) = target(ctx, &username).await? else {
                return Ok(OrganizationOutcome::Denied);
            };
            organizations::add_organization_team_member(pool, actor, org, team, user, role)
                .await
                .map(unit)
        }
        Command::RemoveMember { team, user } => {
            organizations::remove_organization_team_member(pool, actor, org, team, user).await
        }
        Command::Grant {
            team,
            repo,
            permission,
        } => organizations::grant_organization_team_repository(
            pool, actor, org, team, repo, permission,
        )
        .await
        .map(unit),
        Command::Revoke { team, repo } => {
            organizations::revoke_organization_team_repository(pool, actor, org, team, repo).await
        }
    };
    result.map_err(|_| unavailable(ctx))
}

/// The repositories a team grant can name: one page of those the viewer can read. The flag
/// says whether more follow, and then the grant form takes an ID instead of a select.
pub(super) async fn repository_choices(
    ctx: &PageContext,
    actor: i64,
    org: i64,
) -> Result<(Vec<view::RepositoryChoice>, bool), Response> {
    let rows = repository_rows(
        ctx,
        actor,
        org,
        OrganizationPage::new(0, view::REPOSITORY_CHOICE_LIMIT).expect("fixed repository page"),
    )
    .await?;
    // A full page may be the last one, so the next readable repository is probed for.
    let more = match rows.last().map(|row| row.id) {
        Some(last) if rows.len() as i64 == view::REPOSITORY_CHOICE_LIMIT => {
            let probe = OrganizationPage::new(last, 1).expect("fixed probe page");
            !repository_rows(ctx, actor, org, probe).await?.is_empty()
        }
        _ => false,
    };
    let choices = rows
        .into_iter()
        .map(|row| view::RepositoryChoice {
            repo_id: row.id,
            owner: row.owner,
            name: row.name,
        })
        .collect();
    Ok((choices, more))
}

async fn repository_rows(
    ctx: &PageContext,
    actor: i64,
    org: i64,
    page: OrganizationPage,
) -> Result<Vec<OrganizationRepository>, Response> {
    match organizations::list_organization_repositories(&ctx.state.pool, actor, org, page).await {
        Ok(OrganizationOutcome::Applied(rows)) => Ok(rows),
        Ok(_) => Err(not_found(ctx)),
        Err(_) => Err(unavailable(ctx)),
    }
}

pub(super) async fn detail(
    ctx: &PageContext,
    actor: i64,
    org: i64,
) -> Result<Option<view::TeamDetail>, Response> {
    let Some(value) = ctx.query.get("team") else {
        if ["team_members_after", "team_repositories_after"]
            .iter()
            .any(|key| ctx.query.contains_key(*key))
        {
            return Err(invalid(ctx));
        }
        return Ok(None);
    };
    let team: i64 = value
        .parse()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| invalid(ctx))?;
    // A one-row scoped seek works independently of the teams-list cursor. Never
    // scan/truncate all teams or use an unscoped persistence read for this ID.
    let selected = match organizations::list_organization_teams(
        &ctx.state.pool,
        actor,
        org,
        OrganizationPage::new(team - 1, 1).expect("positive team ID"),
    )
    .await
    {
        Ok(OrganizationOutcome::Applied(values)) => {
            values.into_iter().find(|value| value.team.id == team)
        }
        Ok(_) => return Err(not_found(ctx)),
        Err(_) => return Err(unavailable(ctx)),
    }
    .ok_or_else(|| not_found(ctx))?;
    // Legacy hierarchy is unsupported, not a parent ID to expose or repair.
    if selected.team.parent_team_id.is_some() {
        return Err(not_found(ctx));
    }
    let (members_cursor, member_page) =
        organization::cursor(ctx, "team_members_after").ok_or_else(|| invalid(ctx))?;
    let (repositories_cursor, repo_page) =
        organization::cursor(ctx, "team_repositories_after").ok_or_else(|| invalid(ctx))?;
    let members = match organizations::list_organization_team_members(
        &ctx.state.pool,
        actor,
        org,
        team,
        member_page,
    )
    .await
    {
        Ok(OrganizationOutcome::Applied(values)) => values,
        Ok(_) => return Err(not_found(ctx)),
        Err(_) => return Err(unavailable(ctx)),
    };
    // This operation filters foreign/inactive/private repositories by effective
    // actor READ before LIMIT; team visibility alone never reveals grant IDs.
    let repositories = match organizations::list_organization_team_repositories(
        &ctx.state.pool,
        actor,
        org,
        team,
        repo_page,
    )
    .await
    {
        Ok(OrganizationOutcome::Applied(values)) => values,
        Ok(_) => return Err(not_found(ctx)),
        Err(_) => return Err(unavailable(ctx)),
    };
    Ok(Some(view::TeamDetail {
        team: view::Team {
            id: selected.team.id,
            name: selected.team.name,
            slug: selected.team.slug.clone(),
            description: selected.team.description.clone().unwrap_or_default(),
            privacy: selected.team.privacy,
            member_count: selected.member_count,
        },
        slug: selected.team.slug,
        description: selected.team.description.unwrap_or_default(),
        more_members: members.len() as i64 == organization::PAGE_LIMIT,
        more_repositories: repositories.len() as i64 == organization::PAGE_LIMIT,
        members_after: members.last().map(|v| v.id).unwrap_or(members_cursor),
        repositories_after: repositories
            .last()
            .map(|v| v.id)
            .unwrap_or(repositories_cursor),
        members_cursor,
        repositories_cursor,
        members: super::membership::project(
            ctx,
            members.into_iter().map(|v| (v.user_id, v.role)).collect(),
        )
        .await?,
        repositories: repositories
            .into_iter()
            .map(|v| view::RepositoryGrant {
                repo_id: v.repo_id,
                owner: v.owner,
                name: v.name,
                permission: v.permission,
            })
            .collect(),
    }))
}

// Missing organizations, missing teams and teams hidden from the viewer all answer this page.
fn public_team_not_found(ctx: &PageContext) -> Response {
    ctx.error(
        StatusCode::NOT_FOUND,
        "No team has this name, or you cannot see it.",
    )
}

/// The public team page at /orgs/NAME/teams/SLUG, for anyone the store lets see the team.
pub(super) async fn public_show(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, slug)): Path<(String, String)>,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    // Team pages are for signed-in organization members; anonymous viewers get a missing team's answer.
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return public_team_not_found(&ctx);
    };
    let pool = &ctx.state.pool;
    let org = match organizations::get_public_organization(pool, &name).await {
        Ok(Some(org)) => org,
        Ok(None) => return public_team_not_found(&ctx),
        Err(_) => return unavailable(&ctx),
    };
    let public = match organizations::get_public_team(pool, actor, org.id, &slug).await {
        Ok(Some(public)) => public,
        Ok(None) => return public_team_not_found(&ctx),
        Err(_) => return unavailable(&ctx),
    };
    let show_repositories = ctx
        .query
        .get("tab")
        .is_some_and(|tab| tab == "repositories");
    let (after, page) = match organization::cursor(&ctx, "after") {
        Some(value) => value,
        None => return invalid(&ctx),
    };
    let team_id = public.team.id;
    let (members, repositories, more, after) = if show_repositories {
        let rows =
            match organizations::list_public_team_repositories(pool, actor, org.id, team_id, page)
                .await
            {
                Ok(Some(rows)) => rows,
                Ok(None) => return public_team_not_found(&ctx),
                Err(_) => return unavailable(&ctx),
            };
        let more = rows.len() as i64 == organization::PAGE_LIMIT;
        let after = rows.last().map_or(after, |row| row.id);
        let repositories: Vec<view::RepositoryGrant> = rows
            .into_iter()
            .map(|row| view::RepositoryGrant {
                repo_id: row.repo_id,
                owner: row.owner,
                name: row.name,
                permission: row.permission,
            })
            .collect();
        (Vec::new(), repositories, more, after)
    } else {
        let rows = match organizations::list_public_team_members(pool, actor, org.id, team_id, page)
            .await
        {
            Ok(Some(rows)) => rows,
            Ok(None) => return public_team_not_found(&ctx),
            Err(_) => return unavailable(&ctx),
        };
        let more = rows.len() as i64 == organization::PAGE_LIMIT;
        let after = rows.last().map_or(after, |row| row.id);
        let members = match super::membership::project(
            &ctx,
            rows.into_iter()
                .map(|row| (row.user_id, row.role))
                .collect(),
        )
        .await
        {
            Ok(members) => members,
            Err(response) => return response,
        };
        (members, Vec::new(), more, after)
    };
    let PublicTeam {
        team,
        member_count,
        repository_count,
        manage,
    } = public;
    let title = format!("{} / {}", org.name, team.name);
    let model = view::TeamPage {
        organization: view::Organization {
            id: org.id,
            name: org.name,
            display_name: org.display_name,
            description: org.description.unwrap_or_default(),
            suspended: false,
            avatar_url: org
                .avatar_file_id
                .map(|id| format!("/api/files/{id}/download")),
        },
        team: view::Team {
            id: team.id,
            name: team.name,
            slug: team.slug,
            description: team.description.clone().unwrap_or_default(),
            privacy: team.privacy,
            member_count,
        },
        description: team.description.unwrap_or_default(),
        member_count,
        repository_count,
        manage,
        show_repositories,
        members,
        repositories,
        more,
        after,
    };
    ctx.page_model_status(StatusCode::OK, &title, Page::OrganizationTeam(model))
}
