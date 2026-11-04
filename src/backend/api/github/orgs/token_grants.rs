//! Organization owners review and revoke the fine-grained tokens that reach their organization.
//! A request's id and a grant's id are the token id. App and fine-grained tokens cannot use
//! these endpoints.
use super::{outcome, target};
use crate::backend::api::github::api_url;
use crate::backend::api::github::auth;
use crate::backend::api::github::pagination::Pagination;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::fine_grained_tokens::Token;
use crate::backend::db::fine_grained_tokens::TokenFilter;
use crate::backend::db::organizations;
use crate::backend::db::organizations::OrganizationPage;
use crate::backend::identity::fine_grained::catalog;
use crate::backend::identity::fine_grained::Access;
use crate::backend::identity::fine_grained::Group;
use crate::backend::webhooks::payloads;
use axum::{
    extract::{rejection::JsonRejection, OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post, MethodRouter},
    Json,
};
use chrono::{DateTime, Utc};
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::models::Organization;
use codeyard_db_core::models::User;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{collections::HashMap, sync::Arc};

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/list-pat-grant-requests", get(list_requests)),
        ("orgs/review-pat-grant-requests-in-bulk", post(review_many)),
        ("orgs/review-pat-grant-request", post(review_one)),
        (
            "orgs/list-pat-grant-request-repositories",
            get(request_repositories),
        ),
        ("orgs/list-pat-grants", get(list_grants)),
        ("orgs/update-pat-accesses", post(revoke_many)),
        ("orgs/update-pat-access", post(revoke_one)),
        ("orgs/list-pat-grant-repositories", get(grant_repositories)),
    ]
}

/// The organization and the owner acting on it with their own credentials.
async fn context(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    write: bool,
) -> Result<(auth::Authentication, Organization)> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    if write {
        actor.require_write()?;
    }
    if crate::backend::identity::github_apps::authority(&state.pool, actor.token.id)
        .await
        .map_err(Error::unavailable)?
        .is_some()
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Resource not accessible by token",
        ));
    }
    let org = target(state, name).await?;
    owner(state, actor.user.id, org.id).await?;
    Ok((actor, org))
}

/// Fails unless `actor` owns the organization now. Listings check again after reading, so
/// nothing they return was read after the owner lost the organization.
async fn owner(state: &AppState, actor: i64, org: i64) -> Result<()> {
    let role = outcome(
        organizations::get_organization_role(&state.pool, actor, org)
            .await
            .map_err(Error::unavailable)?,
    )?;
    if role != codeyard_db_core::models::OrgRole::Owner {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Must be an organization owner",
        ));
    }
    Ok(())
}

#[derive(Default)]
struct Filters {
    owners: Vec<String>,
    repository: Option<String>,
    permission: Option<(String, Option<String>)>,
    last_used_before: Option<DateTime<Utc>>,
    last_used_after: Option<DateTime<Utc>>,
    token_ids: Vec<i64>,
    ascending: bool,
}

fn filters(uri: &Uri) -> Result<Filters> {
    let invalid = |field: &str| Error::validation("PersonalAccessToken", field, "invalid");
    let mut filters = Filters::default();
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "owner" | "owner[]" => filters.owners.push(value.into_owned()),
            "repository" => filters.repository = Some(value.into_owned()),
            "permission" => {
                // Either a permission name or `name_level`, such as `issues_write`.
                let level = value
                    .rsplit_once('_')
                    .filter(|(name, level)| {
                        matches!(*level, "read" | "write" | "admin") && catalog(name).is_some()
                    })
                    .map(|(name, level)| (name.to_owned(), Some(level.to_owned())));
                let permission = level
                    .or_else(|| catalog(&value).map(|_| (value.clone().into_owned(), None)))
                    .ok_or_else(|| invalid("permission"))?;
                filters.permission = Some(permission);
            }
            "last_used_before" | "last_used_after" => {
                let at = DateTime::parse_from_rfc3339(&value)
                    .map_err(|_| invalid(&key))?
                    .with_timezone(&Utc);
                if key == "last_used_before" {
                    filters.last_used_before = Some(at);
                } else {
                    filters.last_used_after = Some(at);
                }
            }
            "token_id" | "token_id[]" => {
                for id in value.split(',') {
                    filters
                        .token_ids
                        .push(id.trim().parse().map_err(|_| invalid("token_id"))?);
                }
            }
            "sort" if value != "created_at" => return Err(invalid("sort")),
            "direction" => match value.as_ref() {
                "asc" => filters.ascending = true,
                "desc" => filters.ascending = false,
                _ => return Err(invalid("direction")),
            },
            _ => {}
        }
    }
    if filters.owners.len() > 10 {
        return Err(invalid("owner"));
    }
    if filters.token_ids.len() > 50 {
        return Err(invalid("token_id"));
    }
    Ok(filters)
}

/// A page of the organization's tokens in `status` that pass the filters, with their users.
async fn tokens(
    state: &AppState,
    org: &Organization,
    status: &str,
    filters: &Filters,
    offset: usize,
    limit: usize,
) -> Result<Vec<(Token, User)>> {
    let repository = match &filters.repository {
        Some(name) => {
            let Some(repo) =
                db::repositories::records::get_repository(&state.pool, &org.name, name)
                    .await
                    .map_err(Error::unavailable)?
                    .filter(|repo| repo.owner_org_id == Some(org.id) && !repo.is_gist)
            else {
                // No token reaches a repository the organization does not have.
                return Ok(Vec::new());
            };
            Some(repo.id)
        }
        None => None,
    };
    let filter = TokenFilter {
        status,
        owners: &filters.owners,
        token_ids: &filters.token_ids,
        repository,
        permission: filters
            .permission
            .as_ref()
            .map(|(key, level)| (key.as_str(), level.as_deref())),
        last_used_before: filters.last_used_before,
        last_used_after: filters.last_used_after,
        ascending: filters.ascending,
        before: None,
    };
    let rows = db::fine_grained_tokens::organization_tokens(
        &state.pool,
        org.id,
        &filter,
        i64::try_from(offset).map_err(Error::unavailable)?,
        i64::try_from(limit).map_err(Error::unavailable)?,
    )
    .await
    .map_err(Error::unavailable)?;
    let mut ids: Vec<_> = rows.iter().map(|token| token.user_id).collect();
    ids.sort_unstable();
    ids.dedup();
    let users: HashMap<_, _> = db::users::accounts::get_users_by_ids(&state.pool, &ids)
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .map(|user| (user.id, user))
        .collect();
    Ok(rows
        .into_iter()
        .filter_map(|token| {
            let user = users.get(&token.user_id)?.clone();
            Some((token, user))
        })
        .collect())
}

fn permissions(token: &Token) -> Map<String, Value> {
    serde_json::from_str(&token.permissions).unwrap_or_default()
}

fn document(token: &Token, user: &User, org: &Organization, request: bool) -> Value {
    let mut groups = json!({"organization": {}, "repository": {}, "other": {}});
    for (key, level) in permissions(token) {
        let group = match catalog(&key).map(|permission| permission.group) {
            Some(Group::Repository) => "repository",
            Some(Group::Organization) => "organization",
            Some(Group::Account) => "other",
            None => continue,
        };
        groups[group][key] = level;
    }
    let collection = if request {
        "personal-access-token-requests"
    } else {
        "personal-access-tokens"
    };
    let mut value = json!({
        "id": token.token_id,
        "owner": payloads::user(user),
        "repository_selection": Access::parse(&token.repository_access).unwrap_or(Access::Public).selection(),
        "repositories_url": api_url(&format!("/orgs/{}/{collection}/{}/repositories", org.name, token.token_id)),
        "permissions": groups,
        "token_id": token.token_id,
        "token_name": token.name,
        "token_expired": token.expires_at.is_some_and(|at| at <= Utc::now()),
        "token_expires_at": token.expires_at,
        "token_last_used_at": token.last_used_at,
    });
    if request {
        value["reason"] = json!((!token.description.is_empty()).then_some(&token.description));
        value["created_at"] = json!(token.requested_at);
    } else {
        value["access_granted_at"] = json!(token.reviewed_at.unwrap_or(token.requested_at));
    }
    value
}

async fn list(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    page: &Pagination,
    uri: &Uri,
    request: bool,
) -> Result<Response> {
    let (actor, org) = context(state, headers, name, false).await?;
    let filters = filters(uri)?;
    let status = if request { "pending" } else { "approved" };
    let (offset, limit) = page.window()?;
    let rows = tokens(state, &org, status, &filters, offset, limit + 1).await?;
    owner(state, actor.user.id, org.id).await?;
    let more = rows.len() > limit;
    let items = rows
        .iter()
        .take(limit)
        .map(|(token, user)| document(token, user, &org, request))
        .collect();
    page.response(uri, items, more)
}

async fn list_requests(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    list(&state, &headers, &name, &page, &uri, true).await
}

async fn list_grants(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    list(&state, &headers, &name, &page, &uri, false).await
}

/// The repositories a token selected in the organization, or those its user can read for a
/// token with all repositories.
async fn repositories(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    id: i64,
    page: &Pagination,
    uri: &Uri,
    status: &str,
) -> Result<Response> {
    let (actor, org) = context(state, headers, name, false).await?;
    let token = db::fine_grained_tokens::get(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .filter(|token| token.owner_org_id == Some(org.id) && token.status == status)
        .ok_or_else(Error::missing)?;
    let (offset, limit) = page.window()?;
    let mut repositories = Vec::new();
    let mut more = false;
    match Access::parse(&token.repository_access) {
        Some(Access::Selected) => {
            for (repo, _) in db::fine_grained_tokens::repositories(
                &state.pool,
                token.token_id,
                actor.user.id,
                &actor.credential(),
            )
            .await
            .map_err(Error::unavailable)?
            {
                if let Some(repo) =
                    db::repositories::records::get_repository_by_id(&state.pool, repo)
                        .await
                        .map_err(Error::unavailable)?
                        .filter(|repo| {
                            repo.owner_kind == "org" && repo.owner_org_id == Some(org.id)
                        })
                {
                    repositories.push(repo);
                }
            }
            more = repositories.len() > offset + limit;
            repositories = repositories.into_iter().skip(offset).take(limit).collect();
        }
        Some(Access::All) => {
            let window = OrganizationPage::new(offset as i64, limit as i64)
                .map_err(|_| Error::new(StatusCode::BAD_REQUEST, "Invalid page"))?;
            repositories = organizations::access::list_organization_profile_repositories(
                &state.pool,
                org.id,
                Some(token.user_id),
                window,
            )
            .await
            .map_err(Error::unavailable)?;
            let total = organizations::access::count_organization_profile_repositories(
                &state.pool,
                org.id,
                Some(token.user_id),
            )
            .await
            .map_err(Error::unavailable)?;
            more = (offset + limit) < usize::try_from(total).unwrap_or_default();
        }
        _ => {}
    }
    owner(state, actor.user.id, org.id).await?;
    let items = repositories
        .iter()
        .map(|repo| payloads::repository(repo, &org.name))
        .collect();
    page.response(uri, items, more)
}

async fn request_repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    repositories(&state, &headers, &name, id, &page, &uri, "pending").await
}

async fn grant_repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    repositories(&state, &headers, &name, id, &page, &uri, "approved").await
}

#[derive(Deserialize)]
struct Review {
    pat_request_ids: Option<Vec<i64>>,
    action: String,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct Revoke {
    pat_ids: Option<Vec<i64>>,
    action: String,
}

type Body<T> = std::result::Result<Json<T>, JsonRejection>;

/// Approves or denies pending requests. With no ids in the body, every pending request is
/// reviewed. The owner is checked before the body is read.
async fn review(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    id: Option<i64>,
    body: Body<Review>,
) -> Result<u64> {
    let (reviewer, org) = context(state, headers, name, true).await?;
    let invalid = |field: &str| Error::validation("PersonalAccessTokenRequest", field, "invalid");
    let Json(body) = body.map_err(super::super::json_error)?;
    let status = match body.action.as_str() {
        "approve" => "approved",
        "deny" => "denied",
        _ => return Err(invalid("action")),
    };
    let reason = body.reason.filter(|reason| !reason.is_empty());
    if reason
        .as_ref()
        .is_some_and(|reason| reason.len() > 1024 || reason.contains('\0'))
    {
        return Err(invalid("reason"));
    }
    let ids = match (id, body.pat_request_ids) {
        (Some(id), _) => vec![id],
        (None, Some(ids)) if ids.is_empty() || ids.len() > 100 => {
            return Err(invalid("pat_request_ids"))
        }
        (None, Some(ids)) => ids,
        (None, None) => {
            return db::fine_grained_tokens::review_all(
                &state.pool,
                org.id,
                reviewer.database_actor(),
                "pending",
                status,
                reason.as_deref(),
            )
            .await
            .map_err(Error::unavailable)
        }
    };
    db::fine_grained_tokens::review(
        &state.pool,
        org.id,
        reviewer.database_actor(),
        &ids,
        "pending",
        status,
        reason.as_deref(),
    )
    .await
    .map_err(Error::unavailable)
}

async fn review_many(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Body<Review>,
) -> Result<Response> {
    if review(&state, &headers, &name, None, body).await? == 0 {
        return Err(Error::missing());
    }
    Ok((StatusCode::ACCEPTED, Json(json!({}))).into_response())
}

async fn review_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
    body: Body<Review>,
) -> Result<StatusCode> {
    if review(&state, &headers, &name, Some(id), body).await? == 0 {
        return Err(Error::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Revokes approved tokens. A revoked token keeps reading public data only.
async fn revoke(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    id: Option<i64>,
    body: Body<Revoke>,
) -> Result<u64> {
    let (reviewer, org) = context(state, headers, name, true).await?;
    let invalid = |field: &str| Error::validation("PersonalAccessToken", field, "invalid");
    let Json(body) = body.map_err(super::super::json_error)?;
    if body.action != "revoke" {
        return Err(invalid("action"));
    }
    let ids = match id {
        Some(id) => vec![id],
        None => body.pat_ids.unwrap_or_default(),
    };
    if ids.is_empty() || ids.len() > 100 {
        return Err(invalid("pat_ids"));
    }
    db::fine_grained_tokens::review(
        &state.pool,
        org.id,
        reviewer.database_actor(),
        &ids,
        "approved",
        "revoked",
        None,
    )
    .await
    .map_err(Error::unavailable)
}

async fn revoke_many(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Body<Revoke>,
) -> Result<Response> {
    if revoke(&state, &headers, &name, None, body).await? == 0 {
        return Err(Error::missing());
    }
    Ok((StatusCode::ACCEPTED, Json(json!({}))).into_response())
}

async fn revoke_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
    body: Body<Revoke>,
) -> Result<StatusCode> {
    if revoke(&state, &headers, &name, Some(id), body).await? == 0 {
        return Err(Error::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}
