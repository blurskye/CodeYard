//! Organization settings, bounded native commands and authorized page projections.

use super::INDEX;
use crate::backend::api::context::PageContext;
use crate::backend::app::AppState;
use crate::backend::db::models::OrgRole;
use crate::backend::db::organizations;
use crate::backend::db::organizations::OrganizationOutcome::*;
use crate::backend::db::organizations::*;
use crate::backend::repositories::lifecycle;
use crate::webui::app::Page;
use crate::webui::pages::organizations as view;
use axum::extract::rejection::FormRejection;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::collections::HashMap;
use std::sync::Arc;

pub(super) type Fields = HashMap<String, String>;
pub(super) const BODY_LIMIT: usize = 32 * 1024;
/// Refusal for a form that has no more specific reason.
pub(super) const INVALID_FORM: &str = "Check the organization form values.";
/// Query value that makes the organizations index show the deletion notice.
const DELETED_NOTICE: &str = "organization-deleted";

pub(super) enum Command {
    Create(NewOrganization),
    Profile(OrganizationProfile),
    Membership(super::membership::Command),
    Team(super::teams::Command),
    Delete,
}

pub(super) fn field<'a>(form: &'a Fields, key: &str) -> &'a str {
    form.get(key).map(String::as_str).unwrap_or("")
}
pub(super) fn optional<'a>(form: &'a Fields, key: &str) -> Option<&'a str> {
    let value = field(form, key);
    (!value.is_empty()).then_some(value)
}
pub(super) fn id(form: &Fields, key: &str) -> Result<i64, ()> {
    field(form, key).parse().ok().filter(|id| *id > 0).ok_or(())
}
pub(super) fn username(form: &Fields) -> Result<String, ()> {
    let value = field(form, "username");
    if !(3..=1024).contains(&value.len())
        || value.chars().count() > 255
        || !value
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(());
    }
    Ok(value.to_owned())
}
fn keys(action: &str) -> &'static [&'static str] {
    match action {
        "create" => &["slug", "display_name", "description"],
        "profile" => &["display_name", "description"],
        "add-member" => &["username", "role"],
        "member-role" => &["user_id", "role"],
        "remove-member" => &["user_id", "confirm"],
        "recover-owner" => &["org_id", "username", "confirm"],
        "delete-organization" => &["confirm"],
        "team" => &["slug", "name", "description", "privacy"],
        "team-edit" => &["team_id", "name", "description", "privacy"],
        "team-delete" => &["team_id"],
        "team-member" => &["team_id", "username", "role"],
        "remove-team-member" => &["team_id", "user_id", "confirm"],
        "grant" => &["team_id", "repo_id", "permission"],
        "revoke" => &["team_id", "repo_id"],
        _ => &[],
    }
}
fn bound(key: &str) -> usize {
    match key {
        "description" => 4096,
        "slug" | "display_name" | "name" | "username" | "confirm" => 1024,
        "csrf_token" => 64,
        _ => 32,
    }
}

pub(super) fn bounded(form: &Fields) -> bool {
    let allowed = keys(field(form, "action"));
    !allowed.is_empty()
        && form.len() <= allowed.len() + 2
        && form.iter().all(|(key, value)| {
            (matches!(key.as_str(), "csrf_token" | "action") || allowed.contains(&key.as_str()))
                && value.len() <= bound(key)
        })
}

pub(super) fn draft(form: &Fields) -> Fields {
    let action = field(form, "action");
    let allowed = keys(action);
    if allowed.is_empty() {
        return Fields::new();
    }
    // Omit oversized fields, rather than echoing or truncating unknown/secret data.
    form.iter()
        .filter(|(key, value)| {
            (key.as_str() == "action" || allowed.contains(&key.as_str()))
                && value.len() <= bound(key)
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

impl Command {
    pub(super) fn parse(
        form: &Fields,
        current: Option<&OrganizationMetadata>,
    ) -> Result<Self, &'static str> {
        if !bounded(form) {
            return Err(INVALID_FORM);
        }
        let profile = || {
            // A field the request leaves out keeps its stored value; the write reads that
            // value under its own lock. An empty field clears it. Native forms send both.
            let display_name = form
                .contains_key("display_name")
                .then(|| optional(form, "display_name"));
            let description = form
                .contains_key("description")
                .then(|| optional(form, "description"));
            OrganizationProfile::partial(display_name, description).map_err(|_| INVALID_FORM)
        };
        let command = match field(form, "action") {
            "create" if current.is_none() => Self::Create(
                NewOrganization::new(field(form, "slug"), profile()?).map_err(|_| INVALID_FORM)?,
            ),
            "profile" => Self::Profile(profile()?),
            "add-member" | "member-role" | "remove-member" => Self::Membership(
                super::membership::Command::parse(form).map_err(|()| INVALID_FORM)?,
            ),
            "delete-organization" => match current {
                Some(org) if field(form, "confirm") == org.name => Self::Delete,
                _ => return Err("Type the organization name to confirm deletion."),
            },
            _ => Self::Team(super::teams::Command::parse(form)?),
        };
        if current.is_none() && !matches!(command, Self::Create(_)) {
            return Err(INVALID_FORM);
        }
        Ok(command)
    }
}
pub(super) fn selected_id(uri: &Uri) -> Option<i64> {
    uri.path()
        .strip_prefix("/settings/organizations/")
        .and_then(|value| value.parse().ok())
        .filter(|id| *id > 0)
}
pub(super) fn unavailable(ctx: &PageContext) -> Response {
    ctx.error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Organization data is temporarily unavailable.",
    )
}
pub(super) fn invalid(ctx: &PageContext) -> Response {
    ctx.error(StatusCode::BAD_REQUEST, INVALID_FORM)
}
pub(super) fn not_found(ctx: &PageContext) -> Response {
    ctx.error(StatusCode::NOT_FOUND, "Organization not found.")
}
fn metadata(org: OrganizationMetadata) -> view::Organization {
    view::Organization {
        id: org.id,
        name: org.name,
        display_name: org.display_name,
        description: org.description.unwrap_or_default(),
        suspended: org.governance_suspended,
        avatar_url: None,
    }
}

pub(super) async fn selected(
    ctx: &PageContext,
    actor: i64,
    org: i64,
) -> Result<OrganizationMetadata, Response> {
    match organizations::get_organization_metadata(&ctx.state.pool, actor, org).await {
        Ok(Applied(value)) => Ok(value),
        Ok(_) => Err(not_found(ctx)),
        Err(_) => Err(unavailable(ctx)),
    }
}

/// Rows per list page. A full page may have rows after it, so its next link is shown.
pub(super) const PAGE_LIMIT: i64 = 100;

pub(super) fn cursor(ctx: &PageContext, name: &str) -> Option<(i64, OrganizationPage)> {
    cursor_with_limit(ctx, name, PAGE_LIMIT)
}

/// The cursor named by `name`, read as a page of `limit` rows.
pub(super) fn cursor_with_limit(
    ctx: &PageContext,
    name: &str,
    limit: i64,
) -> Option<(i64, OrganizationPage)> {
    let after = match ctx.query.get(name) {
        Some(value) => value.parse().ok()?,
        None => 0,
    };
    OrganizationPage::new(after, limit)
        .ok()
        .map(|page| (after, page))
}

pub(super) async fn respond(
    ctx: &PageContext,
    org: Option<i64>,
    status: StatusCode,
    message: Option<String>,
    draft: Fields,
) -> Response {
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    // Establish visibility before rejecting query values, so missing and
    // unrelated organizations retain the same refusal even with bad cursors.
    let current = if let Some(org) = org {
        let mut value = match selected(ctx, actor, org).await {
            Ok(value) => metadata(value),
            Err(response) => return response,
        };
        value.avatar_url =
            match organizations::get_organization_avatar(&ctx.state.pool, actor, org).await {
                Ok(Applied(avatar)) => avatar.map(|id| format!("/api/files/{id}/download")),
                Ok(_) => return not_found(ctx),
                Err(_) => return unavailable(ctx),
            };
        Some(value)
    } else {
        None
    };
    // The same role check every write makes; the page only chooses which controls to show.
    let can_recover = ctx
        .user
        .as_ref()
        .is_some_and(|user| user.is_admin && !user.is_banned && !user.must_change_password);
    let (can_manage, is_owner) = match org {
        Some(org) => {
            match organizations::get_organization_role(&ctx.state.pool, actor, org).await {
                Ok(Applied(role)) => (administers(role), role == OrgRole::Owner),
                Ok(_) => return not_found(ctx),
                Err(_) => return unavailable(ctx),
            }
        }
        None => (false, false),
    };
    // Owners see how many repositories deleting the organization will also delete.
    let owned_repositories = match org {
        Some(org) if is_owner => {
            match organizations::count_organization_repositories(&ctx.state.pool, actor, org).await
            {
                Ok(Applied(count)) => count,
                _ => return unavailable(ctx),
            }
        }
        _ => 0,
    };
    let manage_access = match ctx.user.as_ref() {
        Some(user) if user.is_admin => {
            match crate::backend::api::user::profile::private_access(ctx).await {
                Ok(allowed) => allowed,
                Err(response) => return response,
            }
        }
        _ => false,
    };
    let (after, page) = match cursor(ctx, "after") {
        Some(page) => page,
        None => return invalid(ctx),
    };
    // Only a site administrator can recover an organization, so only they see the list.
    // The picker needs names only, so profile text stays out of refusal pages.
    let ownerless_organizations = if org.is_none() && can_recover {
        match organizations::list_ownerless_organizations(&ctx.state.pool, actor).await {
            Ok(Applied(values)) => values
                .into_iter()
                .map(|ownerless| view::Organization {
                    id: ownerless.id,
                    name: ownerless.name,
                    display_name: None,
                    description: String::new(),
                    suspended: ownerless.governance_suspended,
                    avatar_url: None,
                })
                .collect(),
            Ok(_) => vec![],
            Err(_) => return unavailable(ctx),
        }
    } else {
        vec![]
    };
    let mut model = view::Page {
        csrf_token: ctx.csrf_token.clone(),
        organizations: vec![],
        selected: current,
        can_manage,
        is_owner,
        viewer_id: actor,
        manage_access,
        tab: requested_tab(ctx),
        members: vec![],
        teams: vec![],
        repositories: vec![],
        more_repositories: false,
        more_organizations: false,
        more_members: false,
        more_teams: false,
        after,
        members_after: 0,
        teams_after: 0,
        // These retain the supplied other-list cursor, not its next-page cursor.
        members_cursor: 0,
        teams_cursor: 0,
        selected_team: None,
        can_recover,
        ownerless_organizations,
        owned_repositories,
        notice: (ctx.query.get("notice").map(String::as_str) == Some(DELETED_NOTICE))
            .then(|| "Organization deleted.".to_owned()),
        message,
        draft,
    };
    if let Some(org) = org {
        model.selected_team = match super::teams::detail(ctx, actor, org).await {
            Ok(value) => value,
            Err(response) => return response,
        };
        let (member_after, member_page) = match cursor(ctx, "members_after") {
            Some(page) => page,
            None => return invalid(ctx),
        };
        let (team_after, team_page) = match cursor(ctx, "teams_after") {
            Some(page) => page,
            None => return invalid(ctx),
        };
        model.members_cursor = member_after;
        model.teams_cursor = team_after;
        model.members = match organizations::list_organization_members(
            &ctx.state.pool,
            actor,
            org,
            member_page,
        )
        .await
        {
            Ok(Applied(values)) => {
                model.members_after = values.last().map(|v| v.id).unwrap_or(member_after);
                model.more_members = values.len() as i64 == PAGE_LIMIT;
                match super::membership::project(
                    ctx,
                    values.into_iter().map(|v| (v.user_id, v.role)).collect(),
                )
                .await
                {
                    Ok(members) => members,
                    Err(response) => return response,
                }
            }
            Ok(_) => return not_found(ctx),
            Err(_) => return unavailable(ctx),
        };
        model.teams =
            match organizations::list_organization_teams(&ctx.state.pool, actor, org, team_page)
                .await
            {
                Ok(Applied(values)) => {
                    model.teams_after = values.last().map(|v| v.team.id).unwrap_or(team_after);
                    model.more_teams = values.len() as i64 == PAGE_LIMIT;
                    values
                        .into_iter()
                        .map(|v| view::Team {
                            id: v.team.id,
                            name: v.team.name,
                            slug: v.team.slug,
                            description: v.team.description.unwrap_or_default(),
                            privacy: v.team.privacy,
                            member_count: v.member_count,
                        })
                        .collect()
                }
                Ok(_) => return not_found(ctx),
                Err(_) => return unavailable(ctx),
            };
        let (repositories, more_repositories) =
            match super::teams::repository_choices(ctx, actor, org).await {
                Ok(choices) => choices,
                Err(response) => return response,
            };
        model.repositories = repositories;
        model.more_repositories = more_repositories;
    } else {
        if ["team", "team_members_after", "team_repositories_after"]
            .iter()
            .any(|key| ctx.query.contains_key(*key))
        {
            return invalid(ctx);
        }
        model.organizations =
            match organizations::list_actor_organizations(&ctx.state.pool, actor, page).await {
                Ok(Applied(values)) => {
                    model.after = values.last().map(|v| v.id).unwrap_or(after);
                    model.more_organizations = values.len() as i64 == PAGE_LIMIT;
                    values.into_iter().map(metadata).collect()
                }
                Ok(_) => {
                    return ctx.error(
                        StatusCode::FORBIDDEN,
                        "This organization operation is not permitted.",
                    )
                }
                Err(_) => return unavailable(ctx),
            };
    }
    // An organization's settings are titled for the organization, not the personal account.
    let title = match &model.selected {
        Some(org) => format!(
            "{} settings",
            org.display_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(&org.name)
        ),
        None => "Organizations".to_owned(),
    };
    ctx.page_model_status(status, &title, Page::Organizations(model))
}

/// The section the URL names. Overview is the default, and any other value shows it too.
pub(super) fn requested_tab(ctx: &PageContext) -> String {
    match ctx.query.get("tab").map(String::as_str) {
        Some(tab @ ("members" | "teams")) => tab.to_owned(),
        _ => "overview".to_owned(),
    }
}

/// Fixed same-origin path with only recognized, valid navigation values.
pub(super) fn destination(ctx: &PageContext, org: Option<i64>, keep_team: bool) -> String {
    let Some(org) = org else {
        return INDEX.to_owned();
    };
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for key in ["members_after", "teams_after"] {
        if ctx.query.contains_key(key) {
            if let Some((after, _)) = cursor(ctx, key) {
                query.append_pair(key, &after.to_string());
            }
        }
    }
    if keep_team {
        if let Some(team) = ctx
            .query
            .get("team")
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|id| *id > 0)
        {
            query.append_pair("team", &team.to_string());
            for key in ["team_members_after", "team_repositories_after"] {
                if ctx.query.contains_key(key) {
                    if let Some((after, _)) = cursor(ctx, key) {
                        query.append_pair(key, &after.to_string());
                    }
                }
            }
        }
    }
    let tab = requested_tab(ctx);
    if tab != "overview" {
        query.append_pair("tab", &tab);
    }
    let query = query.finish();
    format!(
        "{}/{org}{}",
        INDEX,
        if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        }
    )
}
pub(super) fn unit<T>(result: OrganizationOutcome<T>) -> OrganizationOutcome<()> {
    match result {
        Applied(_) => Applied(()),
        Missing => Missing,
        Denied => Denied,
        Conflict => Conflict,
        Suspended => Suspended,
    }
}

/// Deletes each repository the organization owns through the repository deletion path, then
/// the organization. Repositories go first, one at a time, so a retry after a failure only
/// sees what is left. Repositories created meanwhile make the final delete a Conflict.
async fn delete_with_repositories(
    ctx: &PageContext,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<()>, Response> {
    let pool = &ctx.state.pool;
    loop {
        match organizations::next_organization_repository(pool, actor, org).await {
            Ok(Applied(Some(repository))) => {
                if let Err(error) =
                    lifecycle::delete(pool, ctx.state.cluster.as_ref(), actor, repository).await
                {
                    return Err(ctx.error(StatusCode::CONFLICT, error.message));
                }
            }
            Ok(Applied(None)) => break,
            Ok(other) => return Ok(unit(other)),
            Err(_) => return Err(unavailable(ctx)),
        }
    }
    organizations::delete_organization(pool, actor, org)
        .await
        .map(unit)
        .map_err(|_| unavailable(ctx))
}

async fn apply(
    ctx: &PageContext,
    actor: i64,
    org: i64,
    command: Command,
) -> Result<OrganizationOutcome<()>, Response> {
    let pool = &ctx.state.pool;
    // Typed inputs and readable, non-suspended governance were checked before
    // resolving a username. The scoped DAO recheck remains the final authority;
    // no blanket org-admin precheck may replace scoped team-management policy.
    let result = match command {
        Command::Create(_) => return Err(invalid(ctx)),
        Command::Delete => return delete_with_repositories(ctx, actor, org).await,
        Command::Profile(profile) => {
            organizations::update_organization_profile(pool, actor, org, &profile)
                .await
                .map(unit)
        }
        Command::Membership(command) => {
            // An unknown username is the one Missing that the form reports by name.
            let add = matches!(command, super::membership::Command::Add { .. });
            return super::membership::apply(ctx, actor, org, command)
                .await
                .map(|outcome| match outcome {
                    Missing if !add => Denied,
                    outcome => outcome,
                });
        }
        Command::Team(command) => {
            return super::teams::apply(ctx, actor, org, command)
                .await
                .map(|outcome| {
                    if matches!(outcome, Missing) {
                        Denied
                    } else {
                        outcome
                    }
                })
        }
    };
    // Missing scoped targets must not distinguish nonexistent accounts from
    // real but ineligible accounts. The organization's own missing case was
    // already handled by the authorized metadata read.
    result
        .map(|outcome| {
            if matches!(outcome, Missing) {
                Denied
            } else {
                outcome
            }
        })
        .map_err(|_| unavailable(ctx))
}

pub(super) async fn mutate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    form: Result<Form<Fields>, FormRejection>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    let form = match form {
        Ok(Form(form)) => form,
        Err(rejection) => {
            // Do not reflect extractor diagnostics/body values; still use the
            // shared private-cache/security response contract for body errors.
            let status = rejection.status();
            return ctx.error(
                status,
                "Unable to read the organization form. Submit a bounded URL-encoded form.",
            );
        }
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let org = selected_id(&uri);
    if uri.path() != INDEX && org.is_none() {
        return not_found(&ctx);
    }
    let current = if let Some(org) = org {
        match selected(&ctx, actor, org).await {
            Ok(value) => Some(value),
            Err(response) => return response,
        }
    } else {
        None
    };
    let draft = draft(&form);
    if current.as_ref().is_some_and(|org| org.governance_suspended) {
        return respond(
            &ctx,
            org,
            StatusCode::CONFLICT,
            Some("Organization governance is suspended.".into()),
            draft,
        )
        .await;
    }
    let command = match Command::parse(&form, current.as_ref()) {
        Ok(command) => command,
        Err(message) => {
            return respond(
                &ctx,
                org,
                StatusCode::BAD_REQUEST,
                Some(message.into()),
                draft,
            )
            .await
        }
    };
    let self_removal = matches!(&command, Command::Membership(super::membership::Command::Remove { user }) if *user == actor);
    // Every removal needs the removed account's username typed in that row's confirmation field.
    let removed = match &command {
        Command::Membership(super::membership::Command::Remove { user })
        | Command::Team(super::teams::Command::RemoveMember { user, .. }) => Some(*user),
        _ => None,
    };
    if let Some(user) = removed {
        let name =
            match crate::backend::db::users::accounts::get_users_by_ids(&ctx.state.pool, &[user])
                .await
            {
                Ok(users) => users.into_iter().next().map(|user| user.username),
                Err(_) => return unavailable(&ctx),
            };
        if name.as_deref() != Some(field(&form, "confirm")) {
            let message = if self_removal {
                "Type your username to confirm leaving this organization."
            } else {
                "Type the member's username to confirm removing them."
            };
            return respond(
                &ctx,
                org,
                StatusCode::BAD_REQUEST,
                Some(message.into()),
                draft,
            )
            .await;
        }
    }
    // Only a role change or removal can leave an organization without an active owner.
    let membership_change = matches!(
        &command,
        Command::Membership(
            super::membership::Command::ChangeRole { .. }
                | super::membership::Command::Remove { .. }
        )
    );
    let team_deleted = matches!(
        &command,
        Command::Team(super::teams::Command::Delete { .. })
    );
    let organization_deleted = matches!(&command, Command::Delete);
    let outcome = match command {
        Command::Create(input) => {
            organizations::create_organization(&ctx.state.pool, actor, &input)
                .await
                .map(unit)
                .map_err(|_| unavailable(&ctx))
        }
        command => {
            apply(
                &ctx,
                actor,
                org.expect("parsed organization command has a selected organization"),
                command,
            )
            .await
        }
    };
    // An unknown username is the one refusal that names the username.
    if matches!(outcome, Ok(Missing)) && field(&form, "action") == "add-member" {
        let message = format!("No user named {}.", field(&form, "username"));
        return respond(&ctx, org, StatusCode::NOT_FOUND, Some(message), draft).await;
    }
    let (status, message) = match outcome {
        Ok(Applied(())) => {
            let destination = if self_removal {
                INDEX.to_owned()
            } else if organization_deleted {
                format!("{INDEX}?notice={DELETED_NOTICE}")
            } else {
                destination(&ctx, org, !team_deleted)
            };
            return ctx.redirect(&destination);
        }
        Ok(Missing) => (StatusCode::NOT_FOUND, "Organization item not found."),
        Ok(Denied) => (
            StatusCode::FORBIDDEN,
            "This organization operation is not permitted.",
        ),
        Ok(Conflict) if organization_deleted => (
            StatusCode::CONFLICT,
            "New repositories were added while the organization was being deleted. Try again.",
        ),
        Ok(Conflict) if membership_change => (
            StatusCode::CONFLICT,
            "An organization needs at least one active owner.",
        ),
        Ok(Conflict) => (
            StatusCode::CONFLICT,
            "The organization changed or this name is occupied.",
        ),
        Ok(Suspended) => (
            StatusCode::CONFLICT,
            "Organization governance is suspended.",
        ),
        Err(response) => return response,
    };
    respond(&ctx, org, status, Some(message.into()), draft).await
}
pub(super) async fn show(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if uri.path() != INDEX && selected_id(&uri).is_none() {
        return not_found(&ctx);
    }
    respond(&ctx, selected_id(&uri), StatusCode::OK, None, Fields::new()).await
}
