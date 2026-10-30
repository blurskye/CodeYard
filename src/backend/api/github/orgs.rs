//! Organization discovery and memberships use the native tenant authority rules.
mod blocks;
mod external_properties;
mod hooks;
mod immutable_releases;
mod installations;
mod invitations;
mod issue_fields;
mod issue_types;
mod membership;
mod outside;
pub(crate) mod properties;
mod roles;
mod rule_suites;
mod security;
mod settings;
mod token_grants;
use super::{api_url, auth, pagination::Pagination, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        models::{Organization, User},
        organizations::{self, discovery, OrganizationOutcome},
    },
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = vec![
        ("orgs/list", get(list)),
        ("orgs/get", get(get_org)),
        ("orgs/delete", delete(delete_org)),
        ("orgs/list-for-authenticated-user", get(own_orgs)),
        ("orgs/list-for-user", get(user_orgs)),
        ("orgs/list-members", get(members)),
        ("orgs/list-public-members", get(public_members)),
        ("orgs/check-membership-for-user", get(check_member)),
        (
            "orgs/check-public-membership-for-user",
            get(check_public_member),
        ),
        (
            "orgs/set-public-membership-for-authenticated-user",
            put(publicize),
        ),
        (
            "orgs/remove-public-membership-for-authenticated-user",
            delete(conceal),
        ),
        ("orgs/get-membership-for-user", get(membership)),
        (
            "orgs/get-membership-for-authenticated-user",
            get(own_membership),
        ),
        (
            "orgs/list-memberships-for-authenticated-user",
            get(own_memberships),
        ),
        (
            "orgs/update-membership-for-authenticated-user",
            patch(accept),
        ),
    ];
    handlers.extend(membership::handlers());
    handlers.extend(issue_types::handlers());
    handlers.extend(issue_fields::handlers());
    handlers.extend(hooks::handlers());
    handlers.extend(installations::handlers());
    handlers.extend(invitations::handlers());
    handlers.extend(outside::handlers());
    handlers.extend(roles::handlers());
    handlers.push(("repos/get-org-rule-suites", get(rule_suites::list)));
    handlers.push(("repos/get-org-rule-suite", get(rule_suites::get_one)));
    handlers.push((
        "repos/get-repo-rule-suites",
        get(rule_suites::list_repository),
    ));
    handlers.push((
        "repos/get-repo-rule-suite",
        get(rule_suites::get_repository),
    ));
    handlers.extend(blocks::handlers());
    handlers.extend(properties::handlers());
    handlers.extend(security::handlers());
    handlers.extend(settings::handlers());
    handlers.extend(external_properties::handlers());
    handlers.extend(immutable_releases::handlers());
    handlers.extend(token_grants::handlers());
    handlers
}

pub(super) fn outcome<T>(value: OrganizationOutcome<T>) -> Result<T> {
    match value {
        OrganizationOutcome::Applied(value) => Ok(value),
        OrganizationOutcome::Missing => Err(Error::missing()),
        OrganizationOutcome::Denied => Err(Error::new(
            StatusCode::FORBIDDEN,
            "Must have organization access",
        )),
        OrganizationOutcome::Conflict => {
            Err(Error::validation("Organization", "membership", "invalid"))
        }
        OrganizationOutcome::Suspended => Err(Error::new(
            StatusCode::FORBIDDEN,
            "Organization governance is suspended",
        )),
    }
}

pub(super) async fn target(state: &AppState, name: &str) -> Result<Organization> {
    organizations::get_public_organization(&state.pool, name)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)
}

async fn owner(state: &AppState, headers: &HeaderMap, name: &str) -> Result<Organization> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(state, name).await?;
    let role = outcome(
        organizations::get_organization_role(&state.pool, actor.user.id, org.id)
            .await
            .map_err(Error::unavailable)?,
    )?;
    if role != crate::backend::db::models::OrgRole::Owner {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Must be an organization owner",
        ));
    }
    Ok(org)
}

async fn get_org(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, false).await?;
    let org = target(&state, &name).await?;
    let owner = if let Some(actor) = actor {
        matches!(
            organizations::get_organization_role(&state.pool, actor.user.id, org.id)
                .await
                .map_err(Error::unavailable)?,
            OrganizationOutcome::Applied(crate::backend::db::models::OrgRole::Owner)
        )
    } else {
        false
    };
    let configuration = organizations::settings::get(&state.pool, org.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    settings::document(&state, &org, &configuration, owner)
        .await
        .map(Json)
}

pub(super) async fn document(state: &AppState, org: &Organization) -> Result<Value> {
    let configuration = organizations::settings::get(&state.pool, org.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    settings::document(state, org, &configuration, false).await
}

async fn delete_org(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(&state, &name).await?;
    let result = crate::backend::organizations::delete(&state, actor.database_actor(&headers)?, org.id)
        .await
        .map_err(|error| {
            if let Some(error) =
                error.downcast_ref::<crate::backend::repositories::lifecycle::StorageError>()
            {
                Error::new(StatusCode::CONFLICT, error.message)
            } else {
                Error::unavailable(error)
            }
        })?;
    if matches!(result, OrganizationOutcome::Conflict) {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "Organization repositories changed; retry deletion",
        ));
    }
    outcome(result)?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct Since {
    since: Option<i64>,
    per_page: Option<usize>,
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<Since>,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    let limit = query.per_page.unwrap_or(30).clamp(1, 100);
    let mut orgs = discovery::list_organizations(
        &state.pool,
        None,
        false,
        query.since.unwrap_or(0).max(0),
        0,
        (limit + 1) as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = orgs.len() > limit;
    orgs.truncate(limit);
    let mut headers = HeaderMap::new();
    if has_next {
        headers.insert(
            header::LINK,
            format!(
                "<{}?since={}&per_page={limit}>; rel=\"next\"",
                api_url("/organizations"),
                orgs.last().unwrap().id
            )
            .parse()
            .map_err(Error::unavailable)?,
        );
    }
    Ok((
        headers,
        Json(orgs.iter().map(payloads::organization).collect::<Vec<_>>()),
    )
        .into_response())
}

async fn organization_list(
    state: &AppState,
    user: i64,
    private: bool,
    page: Pagination,
    uri: Uri,
) -> Result<Response> {
    let (offset, limit) = page.window()?;
    let rows = discovery::list_organizations(
        &state.pool,
        Some(user),
        private,
        0,
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = rows.len() > limit;
    page.response(
        &uri,
        rows.iter().map(payloads::organization).collect(),
        has_next,
    )
}
async fn own_orgs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    organization_list(&state, actor.user.id, true, page, uri).await
}
async fn user_orgs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    let user = super::users::target(&state, &username).await?;
    // This endpoint only lists public memberships, including when the caller is the target.
    organization_list(&state, user.id, false, page, uri).await
}

#[derive(Deserialize)]
struct Members {
    page: Option<usize>,
    per_page: Option<usize>,
    role: Option<String>,
    filter: Option<String>,
}
async fn member_list(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    query: Members,
    uri: Uri,
    public: bool,
) -> Result<Response> {
    let actor = auth::authenticate(state, headers, false).await?;
    let org = target(state, name).await?;
    let role = query.role.as_deref().unwrap_or("all");
    let filter = query.filter.as_deref().unwrap_or("all");
    if !matches!(role, "all" | "admin" | "member")
        || !matches!(filter, "all" | "2fa_disabled" | "2fa_insecure")
    {
        return Err(Error::validation("Organization", "filter", "invalid"));
    }
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    let rows = outcome(
        discovery::list_visible_members(
            &state.pool,
            actor.as_ref().map(|a| a.user.id),
            org.id,
            public,
            role,
            filter,
            (offset as i64, (limit + 1) as i64),
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    let has_next = rows.len() > limit;
    page.response(&uri, rows.iter().map(payloads::user).collect(), has_next)
}
async fn members(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<Members>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    member_list(&state, &headers, &name, query, uri, false).await
}
async fn public_members(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<Members>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    member_list(&state, &headers, &name, query, uri, true).await
}

async fn check(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    username: &str,
    public: bool,
) -> Result<Response> {
    let actor = auth::authenticate(state, headers, false).await?;
    let org = target(state, name).await?;
    let user = super::users::target(state, username).await?;
    let visible =
        discovery::list_organizations(&state.pool, Some(user.id), false, org.id - 1, 0, 1)
            .await
            .map_err(Error::unavailable)?
            .first()
            .is_some_and(|o| o.id == org.id);
    if visible {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    if !public {
        let is_member = if let Some(actor) = &actor {
            matches!(
                organizations::get_organization_role(&state.pool, actor.user.id, org.id)
                    .await
                    .map_err(Error::unavailable)?,
                OrganizationOutcome::Applied(_)
            )
        } else {
            false
        };
        if !is_member {
            return Ok((
                StatusCode::FOUND,
                [(
                    header::LOCATION,
                    api_url(&format!(
                        "/orgs/{}/public_members/{}",
                        crate::webui::encode_component(name),
                        crate::webui::encode_component(username)
                    )),
                )],
            )
                .into_response());
        }
        if let Some(actor) = actor {
            let states = discovery::list_membership_states(
                &state.pool,
                actor.user.id,
                Some(org.id),
                user.id,
                Some("active"),
                (0, 1),
            )
            .await
            .map_err(Error::unavailable)?;
            if !states.is_empty() {
                return Ok(StatusCode::NO_CONTENT.into_response());
            }
        }
    }
    Ok(StatusCode::NOT_FOUND.into_response())
}
async fn check_member(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
) -> Result<Response> {
    check(&state, &headers, &name, &username, false).await
}
async fn check_public_member(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
) -> Result<Response> {
    check(&state, &headers, &name, &username, true).await
}

async fn visibility(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    username: &str,
    public: bool,
) -> Result<StatusCode> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    if actor.user.username != username {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Cannot change another user's membership visibility",
        ));
    }
    let org = target(state, name).await?;
    outcome(
        discovery::set_membership_public(&state.pool, actor.user.id, org.id, public)
            .await
            .map_err(Error::unavailable)?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}
async fn publicize(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
) -> Result<StatusCode> {
    visibility(&state, &headers, &name, &username, true).await
}
async fn conceal(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
) -> Result<StatusCode> {
    visibility(&state, &headers, &name, &username, false).await
}

fn membership_document(org: &Organization, user: &User, role: &str, state: &str) -> Value {
    let organization = payloads::organization(org);
    json!({"url": api_url(&format!("/orgs/{}/memberships/{}", crate::webui::encode_component(&org.name), crate::webui::encode_component(&user.username))),
        "organization_url": organization["url"], "organization": organization, "user": payloads::user(user),
        "role": if role == "owner" { "admin" } else { "member" }, "state": state, "direct_membership": true})
}
async fn membership_value(
    state: &AppState,
    actor: i64,
    org: &Organization,
    user: &User,
) -> Result<Value> {
    let membership =
        discovery::list_membership_states(&state.pool, actor, Some(org.id), user.id, None, (0, 1))
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .next()
            .ok_or_else(Error::missing)?;
    Ok(membership_document(
        org,
        user,
        &membership.role,
        &membership.state,
    ))
}
async fn membership(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    // Unlike /user/memberships, this read requires membership even for an invitee.
    if !matches!(
        organizations::get_organization_role(&state.pool, actor.user.id, org.id)
            .await
            .map_err(Error::unavailable)?,
        OrganizationOutcome::Applied(_)
    ) {
        return Err(Error::missing());
    }
    let user = super::users::target(&state, &username).await?;
    Ok(Json(
        membership_value(&state, actor.user.id, &org, &user).await?,
    ))
}
async fn own_membership(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    Ok(Json(
        membership_value(&state, actor.user.id, &org, &actor.user).await?,
    ))
}
#[derive(Deserialize)]
struct Memberships {
    page: Option<usize>,
    per_page: Option<usize>,
    state: Option<String>,
}
async fn own_memberships(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<Memberships>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    if query
        .state
        .as_deref()
        .is_some_and(|s| !matches!(s, "active" | "pending"))
    {
        return Err(Error::validation("Membership", "state", "invalid"));
    }
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    let rows = discovery::list_membership_states(
        &state.pool,
        actor.user.id,
        None,
        actor.user.id,
        query.state.as_deref(),
        (offset as i64, (limit + 1) as i64),
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = rows.len() > limit;
    let mut values = Vec::new();
    for row in rows.into_iter().take(limit) {
        let org = organizations::get_public_organization_by_id(&state.pool, row.org_id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        values.push(membership_document(
            &org,
            &actor.user,
            &row.role,
            &row.state,
        ));
    }
    page.response(&uri, values, has_next)
}
#[derive(Deserialize)]
struct Accept {
    state: String,
}
async fn accept(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    input: std::result::Result<Json<Accept>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let Json(input) = input.map_err(super::json_error)?;
    if input.state != "active" {
        return Err(Error::validation("Membership", "state", "invalid"));
    }
    let org = target(&state, &name).await?;
    let member = outcome(
        organizations::accept_organization_invitation(&state.pool, actor.user.id, org.id, None)
            .await
            .map_err(Error::unavailable)?,
    )?;
    Ok(Json(membership_document(
        &org,
        &actor.user,
        &member.role,
        "active",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtered_queries_parse_numeric_pagination() {
        let uri = "/?page=3&per_page=7&role=member&state=pending"
            .parse()
            .unwrap();
        let Query(members) = Query::<Members>::try_from_uri(&uri).unwrap();
        assert_eq!(members.role.as_deref(), Some("member"));
        assert_eq!(
            Pagination {
                page: members.page,
                per_page: members.per_page
            }
            .window()
            .ok(),
            Some((14, 7))
        );
        let Query(memberships) = Query::<Memberships>::try_from_uri(&uri).unwrap();
        assert_eq!(memberships.state.as_deref(), Some("pending"));
        assert_eq!(
            Pagination {
                page: memberships.page,
                per_page: memberships.per_page
            }
            .window()
            .ok(),
            Some((14, 7))
        );
    }
}
