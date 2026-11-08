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
use codeyard_repos_pulls::repositories::content_search::Entry;
use codeyard_repos_pulls::repositories::content_search::KindOfSearch;
use codeyard_repos_pulls::repositories::content_search::Query as SearchQuery;
use codeyard_repos_pulls::repositories::content_search::Sort;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

async fn content(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let kind = if uri.path().ends_with("/code") {
        KindOfSearch::Code
    } else {
        KindOfSearch::Commits
    };
    let authentication = codeyard_identity::github_auth::identity(&state, &headers, false).await?;
    let viewer = authentication.as_ref().map(|a| a.user.id);
    if kind == KindOfSearch::Code && viewer.is_none() {
        return Err(codeyard_identity::github_auth::required());
    }
    let query =
        SearchQuery::parse(kind, q.get("q").map_or("", String::as_str)).map_err(Error::invalid)?;
    if query.needs_viewer() && viewer.is_none() {
        return Err(codeyard_identity::github_auth::required());
    }
    let sort = Sort::parse(kind, q.get("sort").map(String::as_str)).map_err(Error::invalid)?;
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
    let _permit = codeyard_api_http::middleware::wait_for_static_slot(&crate::search::SEARCHES)
        .await
        .ok_or_else(|| {
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Search is busy. Try again shortly.",
            )
        })?;
    if kind == KindOfSearch::Code {
        if let Some(found) = crate::search::global::search(
            &state,
            authentication.as_ref(),
            codeyard_database::cells::search::Kind::Code,
            query.search_filter(),
            q.get("sort").filter(|sort| !sort.is_empty()).cloned(),
            ascending,
            (offset, limit.min(1000 - offset)),
            "contents",
        )
        .await?
        {
            let mut items = Vec::new();
            for hit in found.hits {
                let Some(code) = hit.code else {
                    continue;
                };
                let repository = hit.document;
                let (Some(owner), Some(name)) = (
                    repository["owner"]["login"].as_str(),
                    repository["name"].as_str(),
                ) else {
                    continue;
                };
                let mut value = code_document(
                    &query,
                    owner,
                    name,
                    &code.path,
                    &code.sha,
                    &code.revision,
                    code.size,
                    None,
                    code.fragments
                        .iter()
                        .map(|fragment| (fragment.line, fragment.text.as_str())),
                );
                value["repository"] = repository.clone();
                value["score"] = json!(hit.score);
                items.push(value);
            }
            return Ok((page.headers(&uri,offset+limit<(found.total.max(0) as usize).min(1000))?,Json(json!({"total_count":found.total,"incomplete_results":found.incomplete,"items":items,"search_type":"lexical","indexing":"live"}))).into_response());
        }
    }
    let deadline = tokio::time::Instant::now() + state.search_time_budget;
    let mut results = codeyard_repos_pulls::repositories::content_search::search(
        &state.pool,
        state.cluster.as_ref(),
        viewer,
        &query,
        sort,
        ascending,
    )
    .await
    .map_err(Error::unavailable)?;
    let mut ids = results
        .items
        .iter()
        .map(|hit| hit.scope.repository.id)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    let readable = codeyard_identity::github_auth::readable_repositories(
        &state,
        authentication.as_ref(),
        &ids,
        "contents",
    )
    .await?;
    let previous = results.items.len();
    results
        .items
        .retain(|hit| readable.contains(&hit.scope.repository.id));
    results.incomplete |= previous != results.items.len();
    let total = results.items.len();
    let mut items = Vec::new();
    let mut repositories = HashMap::<i64, Value>::new();
    for hit in results
        .items
        .into_iter()
        .skip(offset)
        .take(limit.min(1000 - offset))
    {
        let repo = &hit.scope.repository;
        let owner = &hit.scope.owner;
        if let std::collections::hash_map::Entry::Vacant(entry) = repositories.entry(repo.id) {
            entry.insert(
                crate::repos::search_document(
                    &state,
                    repo,
                    authentication.as_ref(),
                    deadline,
                    None,
                )
                .await?,
            );
        }
        let repository = repositories
            .get(&repo.id)
            .ok_or_else(|| Error::unavailable(()))?
            .clone();
        let mut value = match hit.entry {
            Entry::Commit(commit) => {
                crate::repos::commits::read_document(&state, repo.id, &commit, owner, &repo.name)
                    .await?
            }
            Entry::Code(code) => code_document(
                &query,
                owner,
                &repo.name,
                &code.path,
                &code.sha,
                &code.revision,
                code.size as i64,
                code.language,
                code.fragments
                    .iter()
                    .map(|fragment| (fragment.line as i64, fragment.text.as_str())),
            ),
        };
        value["repository"] = repository;
        value["score"] = json!(hit.score);
        items.push(value);
    }
    Ok((page.headers(&uri,offset+limit<total.min(1000))?,Json(json!({"total_count":total,"incomplete_results":results.incomplete,"items":items,"search_type":"lexical","indexing":"live"}))).into_response())
}
/// One code search item, as GitHub shapes it.
#[allow(
    clippy::too_many_arguments,
    reason = "A local hit and an index hit fill the same GitHub fields from different sources."
)]
fn code_document<'a>(
    query: &SearchQuery,
    owner: &str,
    name: &str,
    path: &str,
    sha: &str,
    revision: &str,
    size: i64,
    language: Option<&str>,
    fragments: impl Iterator<Item = (i64, &'a str)>,
) -> Value {
    let repo_api = codeyard_types::urls::repo_url(owner, name);
    let encoded = codeyard_types::ref_path::encode_path(path);
    let url = format!("{repo_api}/contents/{encoded}?ref={revision}");
    let (lines, matches): (Vec<_>, Vec<_>) = fragments
        .map(|(line, text)| {
            (
                line.to_string(),
                json!({"object_url":url,"object_type":"FileContent","property":"content","fragment":text,"matches":query.text_matches(text)}),
            )
        })
        .unzip();
    let html = format!(
        "{}/{}/{}",
        codeyard_types::http_config::link_origin(),
        codeyard_types::paths::encode_component(owner),
        codeyard_types::paths::encode_component(name)
    );
    json!({"name":path.rsplit('/').next().unwrap_or(path),"path":path,"sha":sha,"url":url,"git_url":format!("{repo_api}/git/blobs/{sha}"),"html_url":codeyard_types::ref_path::file_url(&html,"blob",revision,path),"file_size":size,"language":language,"line_numbers":lines,"text_matches":matches})
}
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("search/code", get(content)),
        ("search/commits", get(content)),
    ]
}
