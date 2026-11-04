//! Organization teams share native membership, repository and visibility checks.
use codeyard_model::urls::api_url;

use crate::backend::api::github::pagination::next_after;
use crate::backend::api::github::pagination::Pagination;

use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use axum::{
    body::to_bytes,
    extract::{OriginalUri, Path, Query, Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put, MethodRouter},
    Json,
};
use codeyard_db_core::models::Organization;
use codeyard_db_core::models::Team;
use codeyard_db_core::models::User;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{collections::HashMap, sync::Arc};

type Params = HashMap<String, String>;

/// Native team lists are read in keyset batches of this size.
const BATCH: i64 = 100;

fn batch(after: i64) -> Result<codeyard_db_orgs::organizations::OrganizationPage> {
    codeyard_db_orgs::organizations::OrganizationPage::new(after, BATCH)
        .map_err(|_| Error::unavailable(()))
}

fn outcome<T>(
    value: codeyard_db_core::queries::organizations::OrganizationOutcome<T>,
) -> Result<T> {
    match value {
        codeyard_db_core::queries::organizations::OrganizationOutcome::Denied => Err(denied()),
        codeyard_db_core::queries::organizations::OrganizationOutcome::Conflict => {
            Err(Error::validation("Team", "name", "already_exists"))
        }
        other => crate::backend::api::github::orgs::outcome(other),
    }
}

fn denied() -> Error {
    Error::new(StatusCode::FORBIDDEN, "Team mutation denied")
}

fn writer(
    auth: &crate::backend::api::github::auth::Authentication,
) -> codeyard_db_core::queries::credentials::Actor {
    auth.database_actor()
}

async fn actor(
    state: &AppState,
    headers: &HeaderMap,
    method: &Method,
) -> Result<crate::backend::api::github::auth::Authentication> {
    let auth = crate::backend::api::github::auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(crate::backend::api::github::auth::required)?;
    if !crate::backend::api::github::auth::read_method(method) {
        auth.require_write()?;
    }
    Ok(auth)
}

async fn organization(state: &AppState, name: &str) -> Result<Organization> {
    codeyard_db_orgs::organizations::get_public_organization(&state.pool, name)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)
}

async fn selected(
    state: &AppState,
    auth: &crate::backend::api::github::auth::Authentication,
    params: &Params,
) -> Result<(Organization, codeyard_db_orgs::organizations::PublicTeam)> {
    let actor = auth.user.id;
    if let Some(id) = params.get("team_id") {
        let id = id
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(Error::missing)?;
        let team = codeyard_db_orgs::organizations::get_public_team_by_id(&state.pool, actor, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let org = codeyard_db_orgs::organizations::get_public_organization_by_id(
            &state.pool,
            team.team.org_id,
        )
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
        // The legacy route names no organization, so the request middleware cannot apply
        // its token policy.
        crate::backend::api::github::auth::refuse_classic(state, auth.token.id, org.id).await?;
        Ok((org, team))
    } else {
        let org = organization(state, &params["org"]).await?;
        let team = codeyard_db_orgs::organizations::get_public_team(
            &state.pool,
            actor,
            org.id,
            &params["team_slug"],
        )
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
        Ok((org, team))
    }
}

fn team_error(error: anyhow::Error) -> Error {
    if error.is::<codeyard_db_orgs::organizations::InvalidTeamParent>() {
        Error::validation("Team", "parent_team_id", "invalid")
    } else {
        Error::unavailable(error)
    }
}

fn team_value(team: &Team, org: &str, permission: i16, parent: Option<&Team>) -> Value {
    let mut value = crate::backend::webhooks::payloads::team_document(team, org, permission);
    if let Some(parent) = parent {
        value["parent"] = crate::backend::webhooks::payloads::team_document(
            parent,
            org,
            parent.default_permission,
        );
    }
    value
}

/// A team named by a repository record, such as a requested reviewer, with its visible parent.
pub(super) async fn summary(
    state: &AppState,
    team: &Team,
    org: &str,
    permission: i16,
) -> Result<Value> {
    let parent = codeyard_db_orgs::organizations::get_team_parent(&state.pool, team.id)
        .await
        .map_err(Error::unavailable)?;
    Ok(team_value(team, org, permission, parent.as_ref()))
}

async fn parent(
    state: &AppState,
    actor: i64,
    org: i64,
    input: &Map<String, Value>,
) -> Result<Option<Option<i64>>> {
    if let Some(value) = input.get("parent_team_id") {
        return if value.is_null() {
            Ok(Some(None))
        } else {
            value
                .as_i64()
                .filter(|id| *id > 0)
                .map(|id| Some(Some(id)))
                .ok_or_else(|| Error::validation("Team", "parent_team_id", "invalid"))
        };
    }
    if let Some(value) = input.get("parent_team_slug") {
        if value.is_null() {
            return Ok(Some(None));
        }
        let slug = value
            .as_str()
            .ok_or_else(|| Error::validation("Team", "parent_team_slug", "invalid"))?;
        let parent =
            codeyard_db_orgs::organizations::get_public_team(&state.pool, actor, org, slug)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(|| Error::validation("Team", "parent_team_slug", "invalid"))?;
        return Ok(Some(Some(parent.team.id)));
    }
    Ok(None)
}

async fn document(
    state: &AppState,
    org: &Organization,
    public: &codeyard_db_orgs::organizations::PublicTeam,
) -> Result<Value> {
    let team = &public.team;
    let mut value = team_value(
        team,
        &org.name,
        team.default_permission,
        public.parent.as_ref(),
    );
    value["members_count"] = json!(public.member_count);
    value["repos_count"] = json!(public.repository_count);
    value["created_at"] = json!(team.created_at);
    value["updated_at"] = json!(team.updated_at);
    let public_repos = codeyard_db_orgs::organizations::count_organization_profile_repositories(
        &state.pool,
        org.id,
        None,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_organization_projects =
        codeyard_db_orgs::organizations::settings::get(&state.pool, org.id)
            .await
            .map_err(Error::unavailable)?
            .is_none_or(|settings| settings.has_organization_projects);
    value["organization"] = crate::backend::webhooks::payloads::team_organization(
        org,
        public_repos,
        has_organization_projects,
    );
    Ok(value)
}

async fn input(request: Request) -> Result<Map<String, Value>> {
    let bytes = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(crate::backend::api::github::json_bytes_error)?;
    if bytes.is_empty() {
        return Ok(Map::new());
    }
    serde_json::from_slice::<Value>(&bytes)
        .map_err(crate::backend::api::github::json_parse_error)?
        .as_object()
        .cloned()
        .ok_or_else(|| Error::invalid("Expected an object"))
}

fn string<'a>(input: &'a Map<String, Value>, field: &str) -> Result<Option<&'a str>> {
    input
        .get(field)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| Error::validation("Team", field, "invalid"))
        })
        .transpose()
}

fn permission(value: &str) -> Result<codeyard_db_orgs::organizations::TeamRepositoryPermission> {
    codeyard_db_orgs::organizations::TeamRepositoryPermission::new(match value {
        "pull" => 1,
        "triage" => 2,
        "push" => 3,
        "maintain" => 4,
        "admin" => 5,
        _ => return Err(Error::validation("Team", "permission", "invalid")),
    })
    .map_err(|_| Error::validation("Team", "permission", "invalid"))
}

/// GitHub's slug for a team name: accented letters lose their accents, every other character
/// outside ASCII letters, digits and `_` separates words, and words join with `-`.
fn slug(name: &str) -> String {
    use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};
    let mut folded = String::with_capacity(name.len());
    for ch in name.nfkd().filter(|ch| !is_combining_mark(*ch)) {
        match ch {
            'ß' => folded.push_str("ss"),
            'æ' | 'Æ' => folded.push_str("ae"),
            'œ' | 'Œ' => folded.push_str("oe"),
            'ø' | 'Ø' => folded.push('o'),
            'đ' | 'Đ' | 'ð' | 'Ð' => folded.push('d'),
            'ł' | 'Ł' => folded.push('l'),
            'þ' | 'Þ' => folded.push_str("th"),
            'ı' => folded.push('i'),
            ch => folded.push(ch),
        }
    }
    folded
        .to_lowercase()
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// The profile a create or an edit names. An edit changes only the fields the request has.
fn profile(
    input: &Map<String, Value>,
    creating: bool,
    parent: Option<Option<i64>>,
) -> Result<codeyard_db_orgs::organizations::TeamProfile> {
    let refused = |error: codeyard_db_orgs::organizations::OrganizationInputError| {
        Error::validation(
            "Team",
            if error == codeyard_db_orgs::organizations::OrganizationInputError::Description {
                "description"
            } else {
                "name"
            },
            "invalid",
        )
    };
    let mut profile = codeyard_db_orgs::organizations::TeamProfile::default();
    let name = string(input, "name")?;
    match name {
        Some(name) => profile = profile.with_name(name).map_err(refused)?,
        None if creating => return Err(Error::validation("Team", "name", "missing_field")),
        None => {}
    }
    match input.get("description") {
        None => {}
        Some(Value::Null) => profile = profile.with_description(None).map_err(refused)?,
        Some(value) => {
            let description = value
                .as_str()
                .ok_or_else(|| Error::validation("Team", "description", "invalid"))?;
            profile = profile
                .with_description(Some(description))
                .map_err(refused)?;
        }
    }
    let privacy = match string(input, "privacy")? {
        Some("secret") => Some(codeyard_db_orgs::organizations::TeamPrivacy::Secret),
        Some("closed") => Some(codeyard_db_orgs::organizations::TeamPrivacy::Visible),
        Some(_) => return Err(Error::validation("Team", "privacy", "invalid")),
        // A new team is secret unless it has a parent: nested teams must be visible.
        None if creating && parent.flatten().is_some() => {
            Some(codeyard_db_orgs::organizations::TeamPrivacy::Visible)
        }
        None if creating => Some(codeyard_db_orgs::organizations::TeamPrivacy::Secret),
        None => None,
    };
    if let Some(privacy) = privacy {
        profile = profile.with_privacy(privacy);
    }
    let notifications = string(input, "notification_setting")?
        .map(|value| match value {
            "notifications_enabled" => Ok(true),
            "notifications_disabled" => Ok(false),
            _ => Err(Error::validation("Team", "notification_setting", "invalid")),
        })
        .transpose()?;
    let default_permission = string(input, "permission")?
        .map(|value| {
            if !matches!(value, "pull" | "push" | "admin") || (creating && value == "admin") {
                return Err(Error::validation("Team", "permission", "invalid"));
            }
            permission(value)
        })
        .transpose()?;
    let slug = name.map(slug);
    profile
        .with_settings(slug.as_deref(), default_permission, notifications)
        .and_then(|profile| profile.with_parent(parent))
        .map_err(refused)
}

fn strings<'a>(input: &'a Map<String, Value>, field: &str) -> Result<Vec<&'a str>> {
    let Some(value) = input.get(field) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .filter(|values| values.len() <= 100)
        .ok_or_else(|| Error::validation("Team", field, "invalid"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| Error::validation("Team", field, "invalid"))
        })
        .collect()
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    request: Request,
) -> Result<Response> {
    let auth = actor(&state, &headers, &Method::POST).await?;
    let org = organization(&state, &params["org"]).await?;
    // Only owners and admins create teams. Checking first keeps the maintainer and repository
    // names below from telling anyone else which accounts and private repositories exist; the
    // creation checks again.
    match codeyard_db_orgs::organizations::get_organization_role(&state.pool, auth.user.id, org.id)
        .await
        .map_err(Error::unavailable)?
    {
        codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(role)
            if codeyard_db_core::queries::organizations::administers(role) => {}
        codeyard_db_core::queries::organizations::OrganizationOutcome::Missing => {
            return Err(Error::missing())
        }
        _ => return Err(denied()),
    }
    let input = input(request).await?;
    let parent = parent(&state, auth.user.id, org.id, &input).await?;
    let profile = profile(&input, true, parent)?;
    let name = string(&input, "name")?
        .ok_or_else(|| Error::validation("Team", "name", "missing_field"))?;
    let mut maintainers = vec![auth.user.id];
    for username in strings(&input, "maintainers")? {
        maintainers.push(
            codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, username)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(|| Error::validation("Team", "maintainers", "invalid"))?
                .id,
        );
    }
    maintainers.sort_unstable();
    maintainers.dedup();
    let mut repositories = Vec::new();
    for full_name in strings(&input, "repo_names")? {
        let (owner, name) = full_name
            .split_once('/')
            .ok_or_else(|| Error::validation("Team", "repo_names", "invalid"))?;
        let repo =
            codeyard_db_repos::repositories::records::get_repository(&state.pool, owner, name)
                .await
                .map_err(Error::unavailable)?
                .filter(|repo| repo.owner_org_id == Some(org.id))
                .ok_or_else(|| Error::validation("Team", "repo_names", "invalid"))?;
        auth.repository_scope(&state.pool, repo.id, "administration", true)
            .await
            .map_err(crate::backend::api::github::auth::scope_error)?;
        repositories.push(repo.id);
    }
    let new = codeyard_db_orgs::organizations::NewTeam::new(&slug(name), profile)
        .map_err(|_| Error::validation("Team", "name", "invalid"))?
        .with_members_and_repositories(maintainers, repositories);
    let team = outcome(
        codeyard_db_orgs::organizations::create_organization_team(
            &state.pool,
            writer(&auth),
            org.id,
            &new,
        )
        .await
        .map_err(team_error)?,
    )?;
    let public =
        codeyard_db_orgs::organizations::get_public_team_by_id(&state.pool, auth.user.id, team.id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    Ok((
        StatusCode::CREATED,
        Json(document(&state, &org, &public).await?),
    )
        .into_response())
}

async fn team(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    method: Method,
    request: Request,
) -> Result<Response> {
    let auth = actor(&state, &headers, &method).await?;
    let (org, mut public) = selected(&state, &auth, &params).await?;
    if method == Method::DELETE {
        outcome(
            codeyard_db_orgs::organizations::delete_organization_team(
                &state.pool,
                writer(&auth),
                org.id,
                public.team.id,
            )
            .await
            .map_err(Error::unavailable)?,
        )?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    if method == Method::PATCH {
        let input = input(request).await?;
        if params.contains_key("team_id") && !input.contains_key("name") {
            return Err(Error::validation("Team", "name", "missing_field"));
        }
        let parent = parent(&state, auth.user.id, org.id, &input).await?;
        let profile = profile(&input, false, parent)?;
        let team = outcome(
            codeyard_db_orgs::organizations::update_organization_team(
                &state.pool,
                writer(&auth),
                org.id,
                public.team.id,
                &profile,
            )
            .await
            .map_err(team_error)?,
        )?;
        // The counts and the parent follow a new parent, so the whole team is read again.
        public = codeyard_db_orgs::organizations::get_public_team_by_id(
            &state.pool,
            auth.user.id,
            team.id,
        )
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    }
    Ok(Json(document(&state, &org, &public).await?).into_response())
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let auth = actor(&state, &headers, &Method::GET).await?;
    let org = organization(&state, &params["org"]).await?;
    let (state, actor, org_id) = (&*state, auth.user.id, org.id);
    let (rows, more) = page
        .collect(move |after| async move {
            let rows = match codeyard_db_orgs::organizations::list_organization_teams(
                &state.pool,
                actor,
                org_id,
                batch(after)?,
            )
            .await
            .map_err(Error::unavailable)?
            {
                codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(rows) => {
                    rows
                }
                _ => return Err(Error::missing()),
            };
            let next = next_after(&rows, BATCH, |row| row.team.id);
            Ok::<_, Error>((rows, next))
        })
        .await?;
    let items = rows
        .iter()
        .map(|row| {
            team_value(
                &row.team,
                &org.name,
                row.team.default_permission,
                row.parent.as_ref(),
            )
        })
        .collect();
    page.response(&uri, items, more)
}

async fn children(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let auth = actor(&state, &headers, &Method::GET).await?;
    let (org, public) = selected(&state, &auth, &params).await?;
    let (state, actor, org_id, team) = (&*state, auth.user.id, org.id, public.team.id);
    let (rows, more) = page
        .collect(move |after| async move {
            let rows = codeyard_db_orgs::organizations::list_child_teams(
                &state.pool,
                actor,
                org_id,
                team,
                batch(after)?,
            )
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
            let next = next_after(&rows, BATCH, |row| row.id);
            Ok::<_, Error>((rows, next))
        })
        .await?;
    let items = rows
        .iter()
        .map(|row| team_value(row, &org.name, row.default_permission, Some(&public.team)))
        .collect();
    page.response(&uri, items, more)
}

async fn user_teams(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let auth = actor(&state, &headers, &Method::GET).await?;
    let (offset, limit) = page.window()?;
    let rows = codeyard_db_orgs::organizations::list_user_teams(
        &state.pool,
        auth.user.id,
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let more = rows.len() > limit;
    let mut items = Vec::new();
    for team in rows.into_iter().take(limit) {
        // An organization that forbids classic tokens keeps its teams out of their lists too.
        if codeyard_db_identity::fine_grained_tokens::classic_refused(
            &state.pool,
            auth.token.id,
            team.org_id,
        )
        .await
        .map_err(Error::unavailable)?
        {
            continue;
        }
        let Some(org) = codeyard_db_orgs::organizations::get_public_organization_by_id(
            &state.pool,
            team.org_id,
        )
        .await
        .map_err(Error::unavailable)?
        else {
            continue;
        };
        // A team hidden or deleted since the list was read is left out.
        let Some(public) = codeyard_db_orgs::organizations::get_public_team_by_id(
            &state.pool,
            auth.user.id,
            team.id,
        )
        .await
        .map_err(Error::unavailable)?
        else {
            continue;
        };
        items.push(document(&state, &org, &public).await?);
    }
    page.response(&uri, items, more)
}

/// The accounts behind a page of rows, by ID. An account deleted since the rows were read is
/// missing.
async fn users(state: &AppState, ids: impl IntoIterator<Item = i64>) -> Result<HashMap<i64, User>> {
    let ids: Vec<i64> = ids.into_iter().collect();
    Ok(
        codeyard_db_identity::users::accounts::get_users_by_ids(&state.pool, &ids)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .map(|user| (user.id, user))
            .collect(),
    )
}

#[derive(Deserialize)]
struct MembersQuery {
    role: Option<String>,
}

async fn members(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    Query(query): Query<MembersQuery>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let auth = actor(&state, &headers, &Method::GET).await?;
    let (org, public) = selected(&state, &auth, &params).await?;
    let role = match query.role.as_deref().unwrap_or("all") {
        "all" => None,
        value => Some(
            codeyard_db_orgs::organizations::TeamRole::parse(value)
                .map_err(|_| Error::validation("Team", "role", "invalid"))?,
        ),
    };
    let (state, actor, org_id, team) = (&*state, auth.user.id, org.id, public.team.id);
    let (rows, more) = page
        .collect(move |after| async move {
            let rows = codeyard_db_orgs::organizations::list_team_members_by_role(
                &state.pool,
                actor,
                org_id,
                team,
                role,
                batch(after)?,
            )
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
            let next = next_after(&rows, BATCH, |row| row.id);
            Ok::<_, Error>((rows, next))
        })
        .await?;
    let accounts = users(state, rows.iter().map(|row| row.user_id)).await?;
    let items = rows
        .iter()
        .filter_map(|row| accounts.get(&row.user_id))
        .map(crate::backend::webhooks::payloads::user)
        .collect();
    page.response(&uri, items, more)
}

/// The membership routes under `/orgs/{org}/teams/{team_slug}/memberships` and the legacy ones
/// under `/teams/{team_id}/memberships`, which answer with the membership document.
async fn membership(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    method: Method,
    request: Request,
) -> Result<Response> {
    team_member(&state, &headers, &params, &method, request, false).await
}

/// The legacy `/teams/{team_id}/members/{username}` routes, which answer with a bare status and
/// never invite.
async fn legacy_member(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    method: Method,
    request: Request,
) -> Result<Response> {
    team_member(&state, &headers, &params, &method, request, true).await
}

async fn team_member(
    state: &AppState,
    headers: &HeaderMap,
    params: &Params,
    method: &Method,
    request: Request,
    legacy_member: bool,
) -> Result<Response> {
    let auth = actor(state, headers, method).await?;
    let (org, public) = selected(state, &auth, params).await?;
    let username = &params["username"];
    let user = codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(|| {
            if method == Method::PUT {
                Error::validation("Team", "username", "invalid")
            } else {
                Error::missing()
            }
        })?;
    if method == Method::DELETE {
        match codeyard_db_orgs::organizations::remove_organization_team_member(
            &state.pool,
            writer(&auth),
            org.id,
            public.team.id,
            user.id,
        )
        .await
        .map_err(Error::unavailable)?
        {
            codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(())
            | codeyard_db_core::queries::organizations::OrganizationOutcome::Missing => {}
            value => outcome(value)?,
        }
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let (role, membership_state) = if method == Method::PUT {
        let target_role =
            codeyard_db_orgs::organizations::get_organization_role(&state.pool, user.id, org.id)
                .await
                .map_err(Error::unavailable)?;
        let input = input(request).await?;
        let mut role = codeyard_db_orgs::organizations::TeamRole::parse(
            string(&input, "role")?.unwrap_or("member"),
        )
        .map_err(|_| Error::validation("Team", "role", "invalid"))?;
        let codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(target_role) =
            target_role
        else {
            if legacy_member {
                return Err(Error::missing());
            }
            let invitation = outcome(
                codeyard_db_orgs::organizations::invite_team_member(
                    &state.pool,
                    writer(&auth),
                    org.id,
                    public.team.id,
                    user.id,
                    role,
                )
                .await
                .map_err(Error::unavailable)?,
            )?;
            crate::backend::notifications::create::organization_invitation(
                &state.pool,
                &state.notification_broadcaster,
                auth.user.id,
                &invitation,
            )
            .await;
            return Ok(Json(membership_document(
                public.team.id,
                username,
                role.as_str(),
                "pending",
            ))
            .into_response());
        };
        // Organization owners maintain every team they are on.
        if matches!(target_role, codeyard_db_core::models::OrgRole::Owner) {
            role = codeyard_db_orgs::organizations::TeamRole::Maintainer;
        }
        let row = outcome(
            codeyard_db_orgs::organizations::add_organization_team_member(
                &state.pool,
                writer(&auth),
                org.id,
                public.team.id,
                user.id,
                role,
            )
            .await
            .map_err(Error::unavailable)?,
        )?;
        (row.role, "active")
    } else {
        let pending = if legacy_member {
            None
        } else {
            codeyard_db_orgs::organizations::pending_team_membership(
                &state.pool,
                auth.user.id,
                org.id,
                public.team.id,
                user.id,
            )
            .await
            .map_err(Error::unavailable)?
        };
        match pending {
            Some(role) => (role, "pending"),
            None => {
                let member = codeyard_db_orgs::organizations::get_team_member(
                    &state.pool,
                    auth.user.id,
                    org.id,
                    public.team.id,
                    user.id,
                )
                .await
                .map_err(Error::unavailable)?
                .flatten()
                .ok_or_else(Error::missing)?;
                (member.role, "active")
            }
        }
    };
    if legacy_member {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(Json(membership_document(
        public.team.id,
        username,
        &role,
        membership_state,
    ))
    .into_response())
}

fn membership_document(team: i64, username: &str, role: &str, state: &str) -> Value {
    json!({"url":api_url(&format!("/teams/{team}/memberships/{}",codeyard_webui::encode_component(username))),"role":role,"state":state})
}

async fn invitations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let auth = actor(&state, &headers, &Method::GET).await?;
    let (org, public) = selected(&state, &auth, &params).await?;
    let (state, actor, org_id, team) = (&*state, auth.user.id, org.id, public.team.id);
    let (rows, more) = page
        .collect(move |after| async move {
            let rows = outcome(
                codeyard_db_orgs::organizations::list_team_invitations(
                    &state.pool,
                    actor,
                    org_id,
                    team,
                    batch(after)?,
                )
                .await
                .map_err(Error::unavailable)?,
            )?;
            let next = next_after(&rows, BATCH, |row| row.invitation.id);
            Ok::<_, Error>((rows, next))
        })
        .await?;
    let accounts = users(
        state,
        rows.iter().flat_map(|row| {
            [row.invitation.user_id, row.invitation.invited_by_id]
                .into_iter()
                .flatten()
        }),
    )
    .await?;
    let account = |id: Option<i64>| id.and_then(|id| accounts.get(&id));
    let items = rows
        .iter()
        .map(|row| {
            crate::backend::webhooks::payloads::organization_invitation(
                &row.invitation,
                &org,
                account(row.invitation.user_id),
                account(row.invitation.invited_by_id),
                row.team_count,
            )
        })
        .collect();
    page.response(&uri, items, more)
}

fn permissions(level: i16) -> Value {
    json!({"pull":level >= 1,"triage":level >= 2,"push":level >= 3,"maintain":level >= 4,"admin":level >= 5})
}

/// A team's repository with the team's permissions, or `None` when the repository left the
/// organization after the grant was read.
async fn repository_document(
    state: &AppState,
    auth: &crate::backend::api::github::auth::Authentication,
    org: i64,
    grant: &codeyard_db_orgs::organizations::TeamRepositoryGrant,
) -> Result<Option<Value>> {
    let Some(repo) =
        codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, grant.repo_id)
            .await
            .map_err(Error::unavailable)?
            .filter(|repo| repo.owner_org_id == Some(org))
    else {
        return Ok(None);
    };
    let mut value = crate::backend::api::github::repos::document(state, &repo, Some(auth)).await?;
    value["permissions"] = permissions(grant.permission);
    value["role_name"] = json!(match grant.permission {
        1 => "read",
        2 => "triage",
        3 => "write",
        4 => "maintain",
        5 => "admin",
        _ => "none",
    });
    Ok(Some(value))
}

async fn repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let auth = actor(&state, &headers, &Method::GET).await?;
    let (org, public) = selected(&state, &auth, &params).await?;
    let (state, credential, org_id, team) = (&*state, &auth, org.id, public.team.id);
    let (rows, more) = page
        .collect(move |after| async move {
            let rows = codeyard_db_orgs::organizations::list_public_team_repositories(
                &state.pool,
                credential.user.id,
                org_id,
                team,
                batch(after)?,
            )
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
            let next = next_after(&rows, BATCH, |row| row.id);
            // The credential's repository bounds and the reader's current access apply before
            // the page is cut.
            let ids: Vec<i64> = rows.iter().map(|row| row.repo_id).collect();
            let readable = crate::backend::api::github::auth::readable_repositories(
                state,
                Some(credential),
                &ids,
                "metadata",
            )
            .await?;
            let rows = rows
                .into_iter()
                .filter(|row| readable.contains(&row.repo_id))
                .collect();
            Ok::<_, Error>((rows, next))
        })
        .await?;
    let mut items = Vec::new();
    for grant in &rows {
        if let Some(value) = repository_document(state, &auth, org.id, grant).await? {
            items.push(value);
        }
    }
    page.response(&uri, items, more)
}

async fn repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    method: Method,
    request: Request,
) -> Result<Response> {
    let auth = actor(&state, &headers, &method).await?;
    let (org, public) = selected(&state, &auth, &params).await?;
    let (repo, _) = crate::backend::api::github::repository(
        &state,
        &headers,
        &params["owner"],
        &params["repo"],
        !crate::backend::api::github::auth::read_method(&method),
    )
    .await?;
    if repo.owner_org_id != Some(org.id) {
        return Err(Error::missing());
    }
    // The organization's members permission does not reach past the credential's repository
    // bounds: changing a team's access needs administration on that repository.
    let write = !crate::backend::api::github::auth::read_method(&method);
    auth.repository_scope(
        &state.pool,
        repo.id,
        if write { "administration" } else { "metadata" },
        write,
    )
    .await
    .map_err(crate::backend::api::github::auth::scope_error)?;
    if method == Method::PUT {
        let input = input(request).await?;
        let permission = match string(&input, "permission")? {
            Some(value) => permission(value)?,
            None => codeyard_db_orgs::organizations::TeamRepositoryPermission::new(
                public.team.default_permission,
            )
            .map_err(|_| Error::unavailable(()))?,
        };
        outcome(
            codeyard_db_orgs::organizations::grant_organization_team_repository(
                &state.pool,
                writer(&auth),
                org.id,
                public.team.id,
                repo.id,
                permission,
            )
            .await
            .map_err(Error::unavailable)?,
        )?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    if method == Method::DELETE {
        match codeyard_db_orgs::organizations::revoke_organization_team_repository(
            &state.pool,
            writer(&auth),
            org.id,
            public.team.id,
            repo.id,
        )
        .await
        .map_err(Error::unavailable)?
        {
            codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(())
            | codeyard_db_core::queries::organizations::OrganizationOutcome::Missing => {}
            value => outcome(value)?,
        }
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let grant = codeyard_db_orgs::organizations::get_public_team_repository(
        &state.pool,
        auth.user.id,
        org.id,
        public.team.id,
        repo.id,
    )
    .await
    .map_err(Error::unavailable)?
    .flatten()
    .ok_or_else(Error::missing)?;
    if headers
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/vnd.github.v3.repository+json"))
    {
        let value = repository_document(&state, &auth, org.id, &grant)
            .await?
            .ok_or_else(Error::missing)?;
        return Ok(Json(value).into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("teams/list", get(list)),
        ("teams/list-child-in-org", get(children)),
        ("teams/list-child-legacy", get(children)),
        ("teams/create", post(create)),
        ("teams/get-by-name", get(team)),
        ("teams/update-in-org", patch(team)),
        ("teams/delete-in-org", delete(team)),
        ("teams/get-legacy", get(team)),
        ("teams/update-legacy", patch(team)),
        ("teams/delete-legacy", delete(team)),
        ("teams/list-for-authenticated-user", get(user_teams)),
        ("teams/list-pending-invitations-in-org", get(invitations)),
        ("teams/list-pending-invitations-legacy", get(invitations)),
        ("teams/list-members-in-org", get(members)),
        ("teams/list-members-legacy", get(members)),
        ("teams/get-membership-for-user-in-org", get(membership)),
        (
            "teams/add-or-update-membership-for-user-in-org",
            put(membership),
        ),
        (
            "teams/remove-membership-for-user-in-org",
            delete(membership),
        ),
        ("teams/get-member-legacy", get(legacy_member)),
        ("teams/add-member-legacy", put(legacy_member)),
        ("teams/remove-member-legacy", delete(legacy_member)),
        ("teams/get-membership-for-user-legacy", get(membership)),
        (
            "teams/add-or-update-membership-for-user-legacy",
            put(membership),
        ),
        (
            "teams/remove-membership-for-user-legacy",
            delete(membership),
        ),
        ("teams/list-repos-in-org", get(repositories)),
        ("teams/list-repos-legacy", get(repositories)),
        ("teams/check-permissions-for-repo-in-org", get(repository)),
        (
            "teams/add-or-update-repo-permissions-in-org",
            put(repository),
        ),
        ("teams/remove-repo-in-org", delete(repository)),
        ("teams/check-permissions-for-repo-legacy", get(repository)),
        (
            "teams/add-or-update-repo-permissions-legacy",
            put(repository),
        ),
        ("teams/remove-repo-legacy", delete(repository)),
    ]
}
