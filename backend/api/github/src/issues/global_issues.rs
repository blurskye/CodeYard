use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_repos::assignees::AssignedItem;
use codeyard_db_repos::discussion_search::Collection;
use codeyard_db_repos::discussion_search::Relation;
use codeyard_db_repos::discussion_search::Search;
use codeyard_db_repos::discussion_search::Sort;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use std::collections::HashMap;
use std::sync::Arc;

fn boolean(q: &HashMap<String, String>, name: &str, default: bool) -> Result<bool> {
    match q.get(name).map(String::as_str) {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => Err(Error::validation("Issue", name, "invalid")),
    }
}
async fn run(
    state: Arc<AppState>,
    headers: HeaderMap,
    q: HashMap<String, String>,
    page: Pagination,
    uri: Uri,
    organization: Option<String>,
) -> Result<Response> {
    let auth = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let viewer = auth.user.id;
    if let Some(name) = &organization {
        codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, name)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    }
    let relation = match q.get("filter").map_or("assigned", String::as_str) {
        "assigned" => Relation::Assigned,
        "created" => Relation::Created,
        "mentioned" => Relation::Mentioned,
        "subscribed" => Relation::Subscribed,
        "all" | "repos" => Relation::All,
        _ => return Err(Error::validation("Issue", "filter", "invalid")),
    };
    let state_filter = match q.get("state").map_or("open", String::as_str) {
        "all" => None,
        state @ ("open" | "closed") => Some(state.to_owned()),
        _ => return Err(Error::validation("Issue", "state", "invalid")),
    };
    let sort = match q.get("sort").map_or("created", String::as_str) {
        "created" => Sort::Created,
        "updated" => Sort::Updated,
        "comments" => Sort::Comments,
        _ => return Err(Error::validation("Issue", "sort", "invalid")),
    };
    let ascending = match q.get("direction").map_or("desc", String::as_str) {
        "asc" => true,
        "desc" => false,
        _ => return Err(Error::validation("Issue", "direction", "invalid")),
    };
    let labels = q
        .get("labels")
        .map_or("", String::as_str)
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if labels.len() > 100 || labels.iter().any(|s| s.len() > 50) {
        return Err(Error::validation("Issue", "labels", "invalid"));
    }
    let since = q
        .get("since")
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|date| date.with_timezone(&chrono::Utc))
                .map_err(|_| Error::validation("Issue", "since", "invalid"))
        })
        .transpose()?;
    if q.get("type").is_some_and(|s| !s.is_empty()) {
        return Err(Error::validation("Issue", "type", "invalid"));
    }
    let scopes = if ["owned", "collab", "orgs"]
        .iter()
        .any(|key| q.contains_key(*key))
    {
        Some([
            boolean(&q, "owned", false)?,
            boolean(&q, "collab", false)?,
            boolean(&q, "orgs", false)?,
        ])
    } else if q.get("filter").is_some_and(|filter| filter == "repos") {
        Some([true, true, true])
    } else {
        None
    };
    let pulls = boolean(&q, "pulls", true)?;
    let filter = Search::collection(Collection {
        relation,
        state: state_filter,
        labels,
        since,
        organization,
        scopes,
        pulls,
    })
    .map_err(Error::invalid)?
    .with_classic_policy(Some(auth.token.id));
    let (offset, limit) = page.window()?;
    let result = codeyard_db_repos::discussion_search::search(
        &state.pool,
        Some(viewer),
        &filter,
        sort,
        ascending,
        offset as i64,
        limit as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let mut ids = result
        .items
        .iter()
        .map(|(_, repo, _, _)| *repo)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    let readable = codeyard_repos_core::api_shared::github::auth::readable_repositories(
        &state,
        Some(&auth),
        &ids,
        "issues",
    )
    .await?;
    let repos =
        codeyard_db_repos::repositories::records::get_repositories_by_ids(&state.pool, &ids)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .map(|repo| (repo.id, repo))
            .collect::<HashMap<_, _>>();
    let owners =
        codeyard_db_repos::repositories::records::repository_owner_names(&state.pool, &ids)
            .await
            .map_err(Error::unavailable)?;
    let mut items = Vec::new();
    for (id, repo_id, pull_request, _) in result.items {
        if !readable.contains(&repo_id) {
            continue;
        }
        let Some(repo) = repos.get(&repo_id) else {
            continue;
        };
        let owner = owners.get(&repo_id).ok_or_else(|| Error::unavailable(()))?;
        let item = if pull_request {
            AssignedItem::PullRequest(id)
        } else {
            AssignedItem::Issue(id)
        };
        let mut value = codeyard_repos_core::api_shared::github::issues::document(
            &state.pool,
            codeyard_repos_core::api_shared::github::issues::Reader::Request(&state, Some(&auth)),
            repo,
            owner,
            item,
        )
        .await?;
        value["repository"] = codeyard_webhooks::payloads::repository(repo, owner);
        items.push(value);
    }
    page.response(&uri, items, offset + limit < result.total as usize)
}
async fn global(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    run(state, headers, q, page, uri, None).await
}
async fn organization(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
    Query(q): Query<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    run(state, headers, q, page, uri, Some(p["org"].clone())).await
}
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/list", get(global)),
        ("issues/list-for-authenticated-user", get(global)),
        ("issues/list-for-org", get(organization)),
    ]
}
