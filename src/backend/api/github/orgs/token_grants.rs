//! Organization owners review and revoke the fine-grained tokens that reach their organization.
//! A request's id and a grant's id are the token id. App and fine-grained tokens cannot use
//! these endpoints.
use super::{outcome, target};
use crate::backend::{
    api::github::{api_url, auth, pagination::Pagination, Error, Result},
    app::AppState,
    db::{
        self,
        fine_grained_tokens::Token,
        models::{OrgRole, Organization},
        organizations::{self, OrganizationPage},
    },
    identity::fine_grained::{catalog, Access, Group},
    webhooks::payloads,
};
use axum::{
    extract::{rejection::JsonRejection, OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post, MethodRouter},
    Json,
};
use chrono::{DateTime, Utc};
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
    let role = outcome(
        organizations::get_organization_role(&state.pool, actor.user.id, org.id)
            .await
            .map_err(Error::unavailable)?,
    )?;
    if role != OrgRole::Owner {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Must be an organization owner",
        ));
    }
    Ok((actor, org))
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
            "owner" | "owner[]" => filters.owners.push(value.to_lowercase()),
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

/// The organization's tokens in `status` that pass the filters, with their users.
async fn tokens(
    state: &AppState,
    org: &Organization,
    status: &str,
    filters: &Filters,
) -> Result<Vec<(Token, User)>> {
    let mut rows = db::fine_grained_tokens::organization_tokens(&state.pool, org.id, status)
        .await
        .map_err(Error::unavailable)?;
    if filters.ascending {
        rows.reverse();
    }
    let mut ids: Vec<_> = rows.iter().map(|token| token.user_id).collect();
    ids.sort_unstable();
    ids.dedup();
    let users: HashMap<_, _> = db::users::accounts::get_users_by_ids(&state.pool, &ids)
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .map(|user| (user.id, user))
        .collect();
    let repository = match &filters.repository {
        Some(name) => Some(
            db::repositories::records::get_repository(&state.pool, &org.name, name)
                .await
                .map_err(Error::unavailable)?
                .filter(|repo| repo.owner_org_id == Some(org.id) && !repo.is_gist)
                .map(|repo| repo.id),
        ),
        None => None,
    };
    let mut matched = Vec::new();
    for token in rows {
        let Some(user) = users.get(&token.user_id) else {
            continue;
        };
        if !filters.owners.is_empty() && !filters.owners.contains(&user.username.to_lowercase()) {
            continue;
        }
        if !filters.token_ids.is_empty() && !filters.token_ids.contains(&token.token_id) {
            continue;
        }
        if filters
            .last_used_before
            .is_some_and(|before| token.last_used_at.is_none_or(|at| at >= before))
            || filters
                .last_used_after
                .is_some_and(|after| token.last_used_at.is_none_or(|at| at <= after))
        {
            continue;
        }
        if let Some((key, level)) = &filters.permission {
            let permissions = permissions(&token);
            let held = permissions.get(key).and_then(Value::as_str);
            if held.is_none() || (level.is_some() && held != level.as_deref()) {
                continue;
            }
        }
        if let Some(repository) = repository {
            let reaches = match (repository, Access::parse(&token.repository_access)) {
                (Some(_), Some(Access::All)) => true,
                (Some(repo), Some(Access::Selected)) => {
                    db::fine_grained_tokens::selected(&state.pool, token.token_id, repo)
                        .await
                        .map_err(Error::unavailable)?
                }
                _ => false,
            };
            if !reaches {
                continue;
            }
        }
        matched.push((token, user.clone()));
    }
    Ok(matched)
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
    let (_, org) = context(state, headers, name, false).await?;
    let filters = filters(uri)?;
    let status = if request { "pending" } else { "approved" };
    let rows = tokens(state, &org, status, &filters).await?;
    let (offset, limit) = page.window()?;
    let more = rows.len() > offset + limit;
    let items = rows
        .iter()
        .skip(offset)
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
    let (viewer, org) = context(state, headers, name, false).await?;
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
            for (repo, _) in db::fine_grained_tokens::repositories(&state.pool, token.token_id, Some(viewer.user.id))
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
            db::fine_grained_tokens::organization_tokens(&state.pool, org.id, "pending")
                .await
                .map_err(Error::unavailable)?
                .into_iter()
                .map(|token| token.token_id)
                .take(100)
                .collect()
        }
    };
    db::fine_grained_tokens::review(
        &state.pool,
        org.id,
        reviewer.database_actor(headers)?,
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
        reviewer.database_actor(headers)?,
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
