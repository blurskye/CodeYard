use super::{auth, pagination::Pagination, Error, Result};
use crate::backend::{
    app::AppState,
    db,
    repositories::content_search::{self, Entry, KindOfSearch, Query as SearchQuery, Sort},
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
    let authentication = auth::identity(&state, &headers, false).await?;
    let deadline = tokio::time::Instant::now() + state.search_time_budget;
    let viewer = authentication.as_ref().map(|a| a.user.id);
    if kind == KindOfSearch::Code && viewer.is_none() {
        return Err(auth::required());
    }
    let query =
        SearchQuery::parse(kind, q.get("q").map_or("", String::as_str)).map_err(Error::invalid)?;
    if query.needs_viewer() && viewer.is_none() {
        return Err(auth::required());
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
    let _permit =
        crate::backend::api::http::middleware::wait_for_static_slot(&super::search::SEARCHES)
            .await
            .ok_or_else(|| {
                Error::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Search is busy. Try again shortly.",
                )
            })?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut results = content_search::search(
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
    let readable = auth::readable_repositories(&state, authentication.as_ref(), &ids, "contents").await?;
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
                super::repos::search_document(
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
        let repo_api = super::repo_url(owner, &repo.name);
        let mut value = match hit.entry {
            Entry::Commit(commit) => {
                super::commits::read_document(&state, repo.id, &commit, owner, &repo.name).await?
            }
            Entry::Code(code) => {
                let path = super::contents::path_url(&code.path);
                let url = format!("{repo_api}/contents/{path}?ref={}", code.revision);
                let fragments=code.fragments.iter().map(|fragment|json!({"object_url":url,"object_type":"FileContent","property":"content","fragment":fragment.text,"matches":query.text_matches(&fragment.text)})).collect::<Vec<_>>();
                json!({"name":code.path.rsplit('/').next().unwrap_or(&code.path),"path":code.path,"sha":code.sha,"url":url,"git_url":format!("{repo_api}/git/blobs/{}",code.sha),"html_url":crate::webui::pages::repos::ref_path::file_url(&format!("{}/{}/{}",crate::backend::api::http::config::link_origin(),crate::webui::encode_component(owner),crate::webui::encode_component(&repo.name)),"blob",&code.revision,&code.path),"file_size":code.size,"language":code.language,"line_numbers":code.fragments.iter().map(|fragment|fragment.line.to_string()).collect::<Vec<_>>(),"text_matches":fragments})
            }
        };
        value["repository"] = repository;
        value["score"] = json!(hit.score);
        items.push(value);
    }
    Ok((page.headers(&uri,offset+limit<total.min(1000))?,Json(json!({"total_count":total,"incomplete_results":results.incomplete,"items":items,"search_type":"lexical","indexing":"live"}))).into_response())
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("search/code", get(content)),
        ("search/commits", get(content)),
    ]
}
