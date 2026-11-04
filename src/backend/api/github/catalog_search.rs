use crate::backend::api::github::pagination::Pagination;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use codeyard_db_repos::catalog_search::Kind;
use codeyard_db_repos::catalog_search::Options;
use codeyard_db_repos::catalog_search::Search;

use crate::backend::repositories::access::AccessError;

use axum::{
    extract::{OriginalUri, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json,
};
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
async fn catalog(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let authentication =
        crate::backend::api::github::auth::authenticate(&state, &headers, false).await?;
    let viewer = authentication.as_ref().map(|a| a.user.id);
    let kind = match uri.path().rsplit('/').next() {
        Some("repositories") => Kind::Repositories,
        Some("labels") => Kind::Labels,
        Some("users") => Kind::Users,
        Some("topics") => Kind::Topics,
        _ => return Err(Error::missing()),
    };
    let search = Search::parse(kind, q.get("q").map_or("", String::as_str))
        .map_err(Error::invalid)?
        .with_classic_policy(authentication.as_ref().map(|auth| auth.token.id));
    if search.needs_viewer() && viewer.is_none() {
        return Err(crate::backend::api::github::auth::required());
    }
    let ascending = match q.get("order").map_or("desc", String::as_str) {
        "asc" => true,
        "desc" => false,
        _ => return Err(Error::invalid("Invalid search order")),
    };
    search
        .order(q.get("sort").map(String::as_str), ascending)
        .map_err(Error::invalid)?;
    let (offset, limit) = page.window()?;
    if offset >= 1000 {
        return Err(Error::invalid(
            "Only the first 1000 search results are available",
        ));
    }
    let repository = if kind == Kind::Labels {
        let id = q
            .get("repository_id")
            .ok_or_else(|| Error::validation("Search", "repository_id", "missing"))?
            .parse::<i64>()
            .map_err(|_| Error::invalid("Invalid repository_id"))?;
        if id <= 0 {
            return Err(Error::invalid("Invalid repository_id"));
        }
        crate::backend::repositories::access::check_read(&state.pool, id, viewer)
            .await
            .map_err(|e| {
                if e == AccessError::NotFound {
                    Error::missing()
                } else {
                    Error::unavailable(e)
                }
            })?;
        Some(id)
    } else {
        None
    };
    let _permit = crate::backend::api::http::middleware::wait_for_static_slot(
        &crate::backend::api::github::search::SEARCHES,
    )
    .await
    .ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Search is busy. Try again shortly.",
        )
    })?;
    let deadline = tokio::time::Instant::now() + state.search_time_budget;
    let (search, git_incomplete) = if search.needs_git() {
        crate::backend::repositories::catalog_search::prepare(
            &state.pool,
            state.cluster.as_ref(),
            viewer,
            search,
        )
        .await
        .map_err(Error::unavailable)?
    } else {
        (search, false)
    };
    let results = codeyard_db_repos::catalog_search::search(
        &state.pool,
        viewer,
        &search,
        &Options {
            repository,
            sort: q.get("sort").cloned(),
            ascending,
            offset: offset as i64,
            limit: limit.min(1000 - offset) as i64,
        },
    )
    .await
    .map_err(Error::unavailable)?;
    let mut ids: Vec<i64> = results
        .items
        .iter()
        .filter_map(|hit| hit.repository_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let readable = crate::backend::api::github::auth::readable_repositories(
        &state,
        authentication.as_ref(),
        &ids,
        "metadata",
    )
    .await?;
    let mut items = Vec::new();
    let licenses = search.git_licenses();
    let mut incomplete = git_incomplete;
    for hit in results.items {
        let value = match kind {
            Kind::Repositories | Kind::Labels => {
                let id = hit.repository_id.ok_or_else(|| Error::unavailable(()))?;
                if !readable.contains(&id) {
                    incomplete = true;
                    continue;
                }
                let repo =
                    match crate::backend::repositories::access::check_read(&state.pool, id, viewer)
                        .await
                    {
                        Ok(repo) => repo,
                        Err(AccessError::NotFound) => {
                            incomplete = true;
                            continue;
                        }
                        Err(error) => return Err(Error::unavailable(error)),
                    };
                if kind == Kind::Repositories {
                    crate::backend::api::github::repos::search_document(
                        &state,
                        &repo,
                        authentication.as_ref(),
                        deadline,
                        licenses.get(&repo.id).map(|license| license.as_deref()),
                    )
                    .await?
                } else {
                    let id = hit.key.parse().map_err(|_| Error::unavailable(()))?;
                    let Some(label) = codeyard_db_repos::labels::get_label_by_id(&state.pool, id)
                        .await
                        .map_err(Error::unavailable)?
                        .filter(|l| l.repo_id == repo.id)
                    else {
                        incomplete = true;
                        continue;
                    };
                    let owner = codeyard_db_repos::repositories::records::repository_owner_name(
                        &state.pool,
                        repo.id,
                    )
                    .await
                    .map_err(Error::unavailable)?
                    .ok_or_else(|| Error::unavailable(()))?;
                    crate::backend::webhooks::payloads::label(&label, &owner, &repo.name)
                }
            }
            Kind::Users => {
                let (kind, id) = hit
                    .key
                    .split_once(':')
                    .ok_or_else(|| Error::unavailable(()))?;
                let id = id.parse().map_err(|_| Error::unavailable(()))?;
                if kind == "u" {
                    let Some(user) =
                        codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, id)
                            .await
                            .map_err(Error::unavailable)?
                            .filter(|u| !u.is_banned && !u.must_change_password)
                    else {
                        incomplete = true;
                        continue;
                    };
                    let mut value = crate::backend::webhooks::payloads::user(&user);
                    value["name"] = json!(user.display_name);
                    value["bio"] = json!(user.bio);
                    value["location"] = json!(user.location);
                    value["created_at"] = json!(user.created_at.to_rfc3339());
                    value["updated_at"] = json!(user.updated_at.to_rfc3339());
                    value
                } else {
                    let Some(org) = codeyard_db_orgs::organizations::get_public_organization_by_id(
                        &state.pool,
                        id,
                    )
                    .await
                    .map_err(Error::unavailable)?
                    else {
                        incomplete = true;
                        continue;
                    };
                    crate::backend::webhooks::payloads::organization_person(org.id, &org.name)
                }
            }
            Kind::Topics => {
                let Some(topic) = codeyard_db_repos::catalog_search::topic(
                    &state.pool,
                    viewer,
                    &hit.key,
                    authentication.as_ref().map(|auth| auth.token.id),
                )
                .await
                .map_err(Error::unavailable)?
                else {
                    incomplete = true;
                    continue;
                };
                crate::backend::webhooks::payloads::topic(&topic)
            }
        };
        let mut value = value;
        value["score"] = json!(hit.score);
        items.push(value);
    }
    Ok((
        page.headers(&uri, offset + limit < (results.total as usize).min(1000))?,
        Json(json!({"total_count":results.total,"incomplete_results":incomplete,"items":items})),
    )
        .into_response())
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("search/repos", get(catalog)),
        ("search/labels", get(catalog)),
        ("search/users", get(catalog)),
        ("search/topics", get(catalog)),
    ]
}
