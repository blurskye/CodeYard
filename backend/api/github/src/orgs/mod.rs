//! Organization discovery and memberships use the native tenant authority rules.
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_core::models::Organization;
use codeyard_db_core::models::User;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::api_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;
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
    handlers.extend(crate::orgs::membership::handlers());
    handlers.extend(crate::orgs::issue_types::handlers());
    handlers.extend(crate::orgs::issue_fields::handlers());
    handlers.extend(crate::orgs::hooks::handlers());
    handlers.extend(crate::orgs::installations::handlers());
    handlers.extend(crate::orgs::invitations::handlers());
    handlers.extend(crate::orgs::outside::handlers());
    handlers.extend(crate::orgs::roles::handlers());
    handlers.push((
        "repos/get-org-rule-suites",
        get(crate::orgs::rule_suites::list),
    ));
    handlers.push((
        "repos/get-org-rule-suite",
        get(crate::orgs::rule_suites::get_one),
    ));
    handlers.push((
        "repos/get-repo-rule-suites",
        get(crate::orgs::rule_suites::list_repository),
    ));
    handlers.push((
        "repos/get-repo-rule-suite",
        get(crate::orgs::rule_suites::get_repository),
    ));
    handlers.extend(crate::orgs::blocks::handlers());
    handlers.extend(crate::orgs::properties::handlers());
    handlers.extend(crate::orgs::security::handlers());
    handlers.extend(crate::orgs::settings::handlers());
    handlers.extend(crate::orgs::external_properties::handlers());
    handlers.extend(crate::orgs::immutable_releases::handlers());
    handlers.extend(crate::orgs::token_grants::handlers());
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
    codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, name)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)
}

async fn owner_actor(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
) -> Result<(
    Organization,
    codeyard_repos_core::api_shared::github::auth::Authentication,
)> {
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let org = target(state, name).await?;
    let role = outcome(
        codeyard_db_orgs::organizations::membership::get_organization_role(
            &state.pool,
            actor.user.id,
            org.id,
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    if role != codeyard_db_core::models::OrgRole::Owner {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Must be an organization owner",
        ));
    }
    Ok((org, actor))
}

async fn get_org(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let actor =
        codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, false).await?;
    let org = target(&state, &name).await?;
    if let Some(actor) = actor {
        if let OrganizationOutcome::Applied(owned) =
            codeyard_db_orgs::organizations::settings::owned(&state.pool, actor.user.id, org.id)
                .await
                .map_err(Error::unavailable)?
        {
            return crate::orgs::settings::document(
                &state,
                &owned.organization,
                &owned.settings,
                true,
            )
            .await
            .map(Json);
        }
    }
    let configuration = codeyard_db_orgs::organizations::settings::get(&state.pool, org.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    crate::orgs::settings::document(&state, &org, &configuration, false)
        .await
        .map(Json)
}

pub(super) async fn document(state: &AppState, org: &Organization) -> Result<Value> {
    let configuration = codeyard_db_orgs::organizations::settings::get(&state.pool, org.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    crate::orgs::settings::document(state, org, &configuration, false).await
}

async fn delete_org(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    actor.require_write()?;
    let org = target(&state, &name).await?;
    let result = codeyard_app::organizations::delete(&state, actor.database_actor(), org.id)
        .await
        .map_err(|error| {
            if let Some(error) =
                error.downcast_ref::<codeyard_repos_core::repositories::lifecycle::StorageError>()
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
    codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, false).await?;
    let limit = codeyard_api_http::shared::github::pagination::page_size(query.per_page);
    let mut orgs = codeyard_db_orgs::organizations::discovery::list_organizations(
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
        Json(
            orgs.iter()
                .map(codeyard_webhooks::payloads::organization)
                .collect::<Vec<_>>(),
        ),
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
    let rows = codeyard_db_orgs::organizations::discovery::list_organizations(
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
        rows.iter()
            .map(codeyard_webhooks::payloads::organization)
            .collect(),
        has_next,
    )
}
async fn own_orgs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    organization_list(&state, actor.user.id, true, page, uri).await
}
async fn user_orgs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, false).await?;
    let user = crate::users::target(&state, &username).await?;
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
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(state, headers, false).await?;
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
        codeyard_db_orgs::organizations::discovery::list_visible_members(
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
    page.response(
        &uri,
        rows.iter().map(codeyard_webhooks::payloads::user).collect(),
        has_next,
    )
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
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(state, headers, false).await?;
    let org = target(state, name).await?;
    let user = crate::users::target(state, username).await?;
    let visible = codeyard_db_orgs::organizations::discovery::list_organizations(
        &state.pool,
        Some(user.id),
        false,
        org.id - 1,
        0,
        1,
    )
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
                codeyard_db_orgs::organizations::membership::get_organization_role(
                    &state.pool,
                    actor.user.id,
                    org.id
                )
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
                        codeyard_webui::encode_component(name),
                        codeyard_webui::encode_component(username)
                    )),
                )],
            )
                .into_response());
        }
        if let Some(actor) = actor {
            let states = codeyard_db_orgs::organizations::discovery::list_membership_states(
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
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    actor.require_write()?;
    if actor.user.username != username {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Cannot change another user's membership visibility",
        ));
    }
    let org = target(state, name).await?;
    outcome(
        codeyard_db_orgs::organizations::discovery::set_membership_public(
            &state.pool,
            actor.database_actor(),
            org.id,
            public,
        )
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
    let organization = codeyard_webhooks::payloads::organization(org);
    json!({"url": api_url(&format!("/orgs/{}/memberships/{}", codeyard_webui::encode_component(&org.name), codeyard_webui::encode_component(&user.username))),
        "organization_url": organization["url"], "organization": organization, "user": codeyard_webhooks::payloads::user(user),
        "role": if role == "owner" { "admin" } else { "member" }, "state": state, "direct_membership": true})
}
async fn membership_value(
    state: &AppState,
    actor: i64,
    org: &Organization,
    user: &User,
) -> Result<Value> {
    let membership = codeyard_db_orgs::organizations::discovery::list_membership_states(
        &state.pool,
        actor,
        Some(org.id),
        user.id,
        None,
        (0, 1),
    )
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
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let org = target(&state, &name).await?;
    // Unlike /user/memberships, this read requires membership even for an invitee.
    if !matches!(
        codeyard_db_orgs::organizations::membership::get_organization_role(
            &state.pool,
            actor.user.id,
            org.id
        )
        .await
        .map_err(Error::unavailable)?,
        OrganizationOutcome::Applied(_)
    ) {
        return Err(Error::missing());
    }
    let user = crate::users::target(&state, &username).await?;
    Ok(Json(
        membership_value(&state, actor.user.id, &org, &user).await?,
    ))
}
async fn own_membership(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
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
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
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
    let rows = codeyard_db_orgs::organizations::discovery::list_membership_states(
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
        let org = codeyard_db_orgs::organizations::profiles::get_public_organization_by_id(
            &state.pool,
            row.org_id,
        )
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
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    actor.require_write()?;
    let Json(input) = input.map_err(codeyard_model::github_error::json_error)?;
    if input.state != "active" {
        return Err(Error::validation("Membership", "state", "invalid"));
    }
    let org = target(&state, &name).await?;
    let member = outcome(
        codeyard_db_orgs::organizations::invitations::accept_organization_invitation(
            &state.pool,
            actor.database_actor(),
            org.id,
            None,
        )
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

pub mod artifact_metadata;

pub mod attestations;
