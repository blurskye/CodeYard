use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_db_repos::suggestions::ListFilter;
use codeyard_db_repos::suggestions::Refusal;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

type Params = HashMap<String, String>;
pub(super) fn error(error: anyhow::Error) -> Error {
    match codeyard_db_repos::suggestions::refusal(&error) {
        Some(Refusal::Missing) => Error::missing(),
        Some(Refusal::Denied) => Error::new(StatusCode::FORBIDDEN, error.to_string()),
        Some(_) => Error::invalid(error.to_string()),
        None => Error::unavailable(error),
    }
}
#[derive(Deserialize)]
struct Filter {
    #[serde(default = "pending")]
    state: String,
    action: Option<String>,
}
fn pending() -> String {
    "pending".into()
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Query(filter): Query<Filter>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let auth = auth.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let issue = crate::issues::required_issue(&state.pool, repo.id, &p["issue_number"]).await?;
    let (offset, limit) = page.window()?;
    let (mut scanned, mut accepted) = (0_i64, 0_usize);
    let mut rows = Vec::new();
    while rows.len() <= limit {
        let batch = codeyard_db_repos::suggestions::list(
            &state.pool,
            repo.id,
            auth.user.id,
            issue,
            &ListFilter {
                state: &filter.state,
                action: filter.action.as_deref(),
                own: false,
                descending: false,
                offset: scanned,
                limit: 101,
            },
        )
        .await
        .map_err(error)?;
        let done = batch.len() < 101;
        scanned += batch.len() as i64;
        let targets: Vec<_> = batch
            .iter()
            .filter(|row| row.action == "close_issue")
            .filter_map(|row| row.target_id)
            .collect();
        let targets: HashMap<_, _> =
            codeyard_db_repos::issues::issue_repositories(&state.pool, &targets)
                .await
                .map_err(Error::unavailable)?
                .into_iter()
                .collect();
        let repos: Vec<_> = targets.values().copied().collect();
        let readable = codeyard_repos_core::api_shared::github::auth::readable_repositories(
            &state,
            Some(&auth),
            &repos,
            "issues",
        )
        .await?;
        for row in batch {
            if row.action == "close_issue"
                && row
                    .target_id
                    .is_some_and(|id| targets.get(&id).is_none_or(|repo| !readable.contains(repo)))
            {
                continue;
            }
            if accepted >= offset {
                rows.push(row);
            }
            accepted += 1;
            if rows.len() > limit {
                break;
            }
        }
        if done {
            break;
        }
    }
    let next = rows.len() > limit;
    let values = rows
        .iter()
        .take(limit)
        .map(codeyard_webhooks::payloads::issue_suggestion)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Error::unavailable)?;
    page.response(&uri, values, next)
}
async fn resolve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    OriginalUri(uri): OriginalUri,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let authentication = auth.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let actor = &authentication.user;
    let issue = crate::issues::required_issue(&state.pool, repo.id, &p["issue_number"]).await?;
    let id = p["suggestion_id"].parse().map_err(|_| Error::missing())?;
    let row = codeyard_repos_pulls::issues::suggestions::resolve(
        &state.pool,
        &state.notification_broadcaster,
        actor,
        &authentication.credential(),
        (&repo, &p["owner"]),
        (issue, id),
        uri.path().ends_with("/approve"),
    )
    .await
    .map_err(error)?;
    Ok(Json(
        codeyard_webhooks::payloads::issue_suggestion(&row).map_err(Error::unavailable)?,
    ))
}
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/list-suggestions", get(list)),
        ("issues/approve-suggestion", post(resolve)),
        ("issues/dismiss-suggestion", post(resolve)),
    ]
}
