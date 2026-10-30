use super::{auth, issues, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        suggestions::{ListFilter, Refusal},
    },
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
type Params = HashMap<String, String>;
pub(super) fn error(error: anyhow::Error) -> Error {
    match db::suggestions::refusal(&error) {
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
    let actor = auth.ok_or_else(auth::required)?.user;
    let issue = issues::required_issue(&state.pool, repo.id, &p["issue_number"]).await?;
    let (offset, limit) = page.window()?;
    let rows = db::suggestions::list(
        &state.pool,
        repo.id,
        actor.id,
        issue,
        &ListFilter {
            state: &filter.state,
            action: filter.action.as_deref(),
            own: false,
            descending: false,
            offset: offset as i64,
            limit: (limit + 1) as i64,
        },
    )
    .await
    .map_err(error)?;
    let next = rows.len() > limit;
    let values = rows
        .iter()
        .take(limit)
        .map(payloads::issue_suggestion)
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
    let authentication = auth.ok_or_else(auth::required)?;
    let actor = &authentication.user;
    let issue = issues::required_issue(&state.pool, repo.id, &p["issue_number"]).await?;
    let id = p["suggestion_id"].parse().map_err(|_| Error::missing())?;
    let row = crate::backend::issues::suggestions::resolve(
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
        payloads::issue_suggestion(&row).map_err(Error::unavailable)?,
    ))
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/list-suggestions", get(list)),
        ("issues/approve-suggestion", post(resolve)),
        ("issues/dismiss-suggestion", post(resolve)),
    ]
}
