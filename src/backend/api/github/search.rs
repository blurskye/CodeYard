use super::{auth, issues, pagination::Pagination, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        assignees::AssignedItem,
        discussion_search::{Search, Sort},
    },
};
use axum::{
    extract::{OriginalUri, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::Arc,
};

pub(in crate::backend::api) static SEARCHES: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(4);

async fn search_issues(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let auth = auth::authenticate(&state, &headers, false).await?;
    let viewer = auth.as_ref().map(|auth| auth.user.id);
    let query = Search::parse(q.get("q").map_or("", String::as_str)).map_err(Error::invalid)?
        .with_classic_policy(auth.as_ref().map(|auth| auth.token.id));
    if query.needs_viewer() && viewer.is_none() {
        return Err(auth::required());
    }
    let sort = Sort::parse(q.get("sort").map(String::as_str)).map_err(Error::invalid)?;
    let ascending = match q.get("order").map_or("desc", String::as_str) {
        "asc" => true,
        "desc" => false,
        _ => return Err(Error::invalid("Invalid search order")),
    };
    let (offset, limit) = page.window()?;
    if offset >= 1000 {
        return Err(Error::invalid(
            "Only the first 1000 search results are available",
        ));
    }
    let search_type = q.get("search_type").map_or("lexical", String::as_str);
    if search_type != "lexical" && viewer.is_none() {
        return Err(auth::required());
    }
    if !matches!(search_type, "lexical" | "semantic" | "hybrid") {
        return Err(Error::validation("Search", "search_type", "invalid"));
    }
    let _permit = crate::backend::api::http::middleware::wait_for_static_slot(&SEARCHES)
        .await
        .ok_or_else(|| {
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Search is busy. Try again shortly.",
            )
        })?;
    let results = db::discussion_search::search(
        &state.pool,
        viewer,
        &query,
        sort,
        ascending,
        offset as i64,
        limit.min(1000 - offset) as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let mut repo_ids = results
        .items
        .iter()
        .map(|(_, repo, _, _)| *repo)
        .collect::<Vec<_>>();
    repo_ids.sort_unstable();
    repo_ids.dedup();
    // Reauthorize the page after the search snapshot, before assembling any private documents.
    let readable = auth::readable_repositories(&state, auth.as_ref(), &repo_ids, "issues").await?;
    let repos = db::repositories::records::get_repositories_by_ids(&state.pool, &repo_ids)
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .map(|repo| (repo.id, repo))
        .collect::<HashMap<_, _>>();
    let owners = db::repositories::records::repository_owner_names(&state.pool, &repo_ids)
        .await
        .map_err(Error::unavailable)?;
    let mut items = Vec::<Value>::new();
    let mut incomplete = false;
    for (id, repo_id, pull_request, score) in results.items {
        if !readable.contains(&repo_id) {
            incomplete = true;
            continue;
        }
        let Some(repo) = repos.get(&repo_id) else {
            incomplete = true;
            continue;
        };
        let owner = owners.get(&repo_id).ok_or_else(|| Error::unavailable(()))?;
        let item = if pull_request {
            AssignedItem::PullRequest(id)
        } else {
            AssignedItem::Issue(id)
        };
        let mut value = issues::document(&state.pool, issues::Reader::Request(&state, auth.as_ref()), repo, owner, item).await?;
        value["score"] = json!(score);
        items.push(value);
    }
    let has_next = (offset + limit) < (results.total as usize).min(1000);
    let mut body = json!({"total_count":results.total,"incomplete_results":incomplete,"items":items,"search_type":"lexical"});
    if search_type != "lexical" {
        // There is no semantic index on this server. The GitHub contract explicitly permits an
        // advertised lexical fallback; never label lexical matches as semantic results.
        body["lexical_fallback_reason"] = json!(["service_unavailable"]);
    }
    Ok((page.headers(&uri, has_next)?, Json(body)).into_response())
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("search/issues-and-pull-requests", get(search_issues))]
}
