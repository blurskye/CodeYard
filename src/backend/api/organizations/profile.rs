//! The public page of an organization, at the same address as a user profile.
//! The store decides what is visible; this page renders only what it returns.

use super::membership::project;
use super::organization::{cursor_with_limit, invalid, unavailable};
use crate::backend::api::http::context::PageContext;
use crate::backend::api::users::profile::profile_repository;
use crate::backend::db::organizations;
use crate::backend::db::organizations::OrganizationOutcome;
use crate::webui::app::Page;
use crate::webui::pages::orgs::{Organization, OrganizationProfile, Team};
use axum::http::StatusCode;
use axum::response::Response;

/// Rows per page of each tab.
const TAB_PAGE_LIMIT: i64 = 30;

pub(crate) async fn public_profile_response(ctx: &PageContext, name: &str) -> Response {
    let pool = &ctx.state.pool;
    let org = match organizations::get_public_organization(pool, name).await {
        Ok(Some(org)) => org,
        // The same answer as an unknown user, so the two cannot be told apart.
        Ok(None) => {
            return ctx.error(
                StatusCode::NOT_FOUND,
                "No page, user or organization has this name.",
            )
        }
        Err(_) => return unavailable(ctx),
    };
    let viewer = ctx.user.as_ref().map(|user| user.id);
    let repository_count =
        match organizations::count_organization_profile_repositories(pool, org.id, viewer).await {
            Ok(count) => count,
            Err(_) => return unavailable(ctx),
        };
    // Only owners and admins are offered the management page; members are counted, others are not.
    let (manage, member_count, team_count) = match viewer {
        Some(actor) => match organizations::get_organization_role(pool, actor, org.id).await {
            Ok(OrganizationOutcome::Applied(role)) => {
                let members =
                    match organizations::count_organization_members(pool, actor, org.id).await {
                        Ok(OrganizationOutcome::Applied(count)) => Some(count),
                        Ok(_) => None,
                        Err(_) => return unavailable(ctx),
                    };
                let teams = match members {
                    Some(_) => {
                        match organizations::count_organization_teams(pool, actor, org.id).await {
                            Ok(OrganizationOutcome::Applied(count)) => Some(count),
                            Ok(_) => None,
                            Err(_) => return unavailable(ctx),
                        }
                    }
                    None => None,
                };
                (organizations::administers(role), members, teams)
            }
            Ok(_) => (false, None, None),
            Err(_) => return unavailable(ctx),
        },
        None => (false, None, None),
    };
    // The member list, the member count and the team list are for members only, so a
    // count is present exactly when the People and Teams tabs may be shown.
    let members_visible = member_count.is_some();
    let tab = match ctx.query.get("tab").map(String::as_str) {
        Some("people") if members_visible => "people",
        Some("teams") if members_visible => "teams",
        _ => "repositories",
    };
    let mut repositories = Vec::new();
    let (people, teams, more, after) = match (tab, viewer) {
        ("repositories", _) => {
            let (after, page) = if ctx
                .query
                .get("tab")
                .is_some_and(|tab| matches!(tab.as_str(), "people" | "teams"))
            {
                (
                    0,
                    organizations::OrganizationPage::new(0, TAB_PAGE_LIMIT)
                        .expect("valid page limit"),
                )
            } else {
                let Some(page) = cursor_with_limit(ctx, "after", TAB_PAGE_LIMIT) else {
                    return invalid(ctx);
                };
                page
            };
            let rows = match organizations::list_organization_profile_repositories(
                pool, org.id, viewer, page,
            )
            .await
            {
                Ok(rows) => rows,
                Err(_) => return unavailable(ctx),
            };
            let after = after + rows.len() as i64;
            let more = after < repository_count;
            repositories = rows.into_iter().map(profile_repository).collect();
            (Vec::new(), Vec::new(), more, after)
        }
        ("people", Some(actor)) => {
            let Some((after, page)) = cursor_with_limit(ctx, "after", TAB_PAGE_LIMIT) else {
                return invalid(ctx);
            };
            let rows =
                match organizations::list_organization_members(pool, actor, org.id, page).await {
                    Ok(OrganizationOutcome::Applied(rows)) => rows,
                    _ => return unavailable(ctx),
                };
            let more = rows.len() as i64 == TAB_PAGE_LIMIT;
            let after = rows.last().map_or(after, |row| row.id);
            let people = match project(
                ctx,
                rows.into_iter()
                    .map(|row| (row.user_id, row.role))
                    .collect(),
            )
            .await
            {
                Ok(people) => people,
                Err(response) => return response,
            };
            (people, Vec::new(), more, after)
        }
        ("teams", Some(actor)) => {
            let Some((after, page)) = cursor_with_limit(ctx, "after", TAB_PAGE_LIMIT) else {
                return invalid(ctx);
            };
            let rows = match organizations::list_organization_teams(pool, actor, org.id, page).await
            {
                Ok(OrganizationOutcome::Applied(rows)) => rows,
                _ => return unavailable(ctx),
            };
            let more = rows.len() as i64 == TAB_PAGE_LIMIT;
            let after = rows.last().map_or(after, |row| row.team.id);
            let teams: Vec<Team> = rows
                .into_iter()
                .map(|row| Team {
                    id: row.team.id,
                    name: row.team.name,
                    slug: row.team.slug,
                    description: row.team.description.unwrap_or_default(),
                    privacy: row.team.privacy,
                    member_count: row.member_count,
                })
                .collect();
            (Vec::new(), teams, more, after)
        }
        _ => (Vec::new(), Vec::new(), false, 0),
    };
    let title = org.name.clone();
    let page = OrganizationProfile {
        organization: Organization {
            id: org.id,
            name: org.name,
            display_name: org.display_name,
            description: org.description.unwrap_or_default(),
            suspended: false,
            avatar_url: org
                .avatar_file_id
                .map(|id| format!("/api/files/{id}/download")),
        },
        repositories,
        repository_count,
        member_count,
        team_count,
        manage,
        tab: tab.to_owned(),
        people,
        teams,
        more,
        after,
    };
    ctx.page_model_status(StatusCode::OK, &title, Page::OrganizationProfile(page))
}
