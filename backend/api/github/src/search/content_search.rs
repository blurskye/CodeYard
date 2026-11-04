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
use codeyard_app::repositories::content_search::Entry;
use codeyard_app::repositories::content_search::KindOfSearch;
use codeyard_app::repositories::content_search::Query as SearchQuery;
use codeyard_app::repositories::content_search::Sort;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
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
    let authentication =
        codeyard_app::api_shared::github::auth::identity(&state, &headers, false).await?;
    let viewer = authentication.as_ref().map(|a| a.user.id);
    if kind == KindOfSearch::Code && viewer.is_none() {
        return Err(codeyard_app::api_shared::github::auth::required());
    }
    let query =
        SearchQuery::parse(kind, q.get("q").map_or("", String::as_str)).map_err(Error::invalid)?;
    if query.needs_viewer() && viewer.is_none() {
        return Err(codeyard_app::api_shared::github::auth::required());
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
    let deadline = tokio::time::Instant::now() + state.search_time_budget;
    let mut results = codeyard_app::repositories::content_search::search(
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
    let readable = codeyard_app::api_shared::github::auth::readable_repositories(
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
        let repo_api = codeyard_model::urls::repo_url(owner, &repo.name);
        let mut value = match hit.entry {
            Entry::Commit(commit) => {
                crate::repos::commits::read_document(&state, repo.id, &commit, owner, &repo.name)
                    .await?
            }
            Entry::Code(code) => {
                let path = codeyard_webui::pages::repos::ref_path::encode_path(&code.path);
                let url = format!("{repo_api}/contents/{path}?ref={}", code.revision);
                let fragments=code.fragments.iter().map(|fragment|json!({"object_url":url,"object_type":"FileContent","property":"content","fragment":fragment.text,"matches":query.text_matches(&fragment.text)})).collect::<Vec<_>>();
                json!({"name":code.path.rsplit('/').next().unwrap_or(&code.path),"path":code.path,"sha":code.sha,"url":url,"git_url":format!("{repo_api}/git/blobs/{}",code.sha),"html_url":codeyard_webui::pages::repos::ref_path::file_url(&format!("{}/{}/{}",codeyard_model::http_config::link_origin(),codeyard_webui::encode_component(owner),codeyard_webui::encode_component(&repo.name)),"blob",&code.revision,&code.path),"file_size":code.size,"language":code.language,"line_numbers":code.fragments.iter().map(|fragment|fragment.line.to_string()).collect::<Vec<_>>(),"text_matches":fragments})
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
