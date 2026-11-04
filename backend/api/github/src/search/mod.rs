use axum::extract::OriginalUri;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_repos::assignees::AssignedItem;
use codeyard_db_repos::discussion_search::Search;
use codeyard_db_repos::discussion_search::Sort;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub static SEARCHES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

async fn search_issues(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let auth = codeyard_identity::github_auth::authenticate(&state, &headers, false).await?;
    let viewer = auth.as_ref().map(|auth| auth.user.id);
    let query = Search::parse(q.get("q").map_or("", String::as_str))
        .map_err(Error::invalid)?
        .with_classic_policy(auth.as_ref().map(|auth| auth.token.id));
    if query.needs_viewer() && viewer.is_none() {
        return Err(codeyard_identity::github_auth::required());
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
        return Err(codeyard_identity::github_auth::required());
    }
    if !matches!(search_type, "lexical" | "semantic" | "hybrid") {
        return Err(Error::validation("Search", "search_type", "invalid"));
    }
    let _permit = codeyard_api_http::middleware::wait_for_static_slot(&SEARCHES)
        .await
        .ok_or_else(|| {
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Search is busy. Try again shortly.",
            )
        })?;
    let results = codeyard_db_repos::discussion_search::search(
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
    let readable = codeyard_identity::github_auth::readable_repositories(
        &state,
        auth.as_ref(),
        &repo_ids,
        "issues",
    )
    .await?;
    let repos =
        codeyard_db_repos::repositories::records::get_repositories_by_ids(&state.pool, &repo_ids)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .map(|repo| (repo.id, repo))
            .collect::<HashMap<_, _>>();
    let owners =
        codeyard_db_repos::repositories::records::repository_owner_names(&state.pool, &repo_ids)
            .await
            .map_err(Error::unavailable)?;
    let mut selected = Vec::new();
    let mut scores = Vec::new();
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
        selected.push((repo, owner.as_str(), item));
        scores.push(score);
    }
    let mut items = Vec::<Value>::new();
    for (mut value, score) in crate::issues::documents_across(
        &state.pool,
        codeyard_webhooks::issue_documents::Reader::Request(&state, auth.as_ref()),
        &selected,
    )
    .await?
    .into_iter()
    .zip(scores)
    {
        if value.is_null() {
            incomplete = true;
            continue;
        }
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

pub mod content_search;

pub mod catalog_search;
