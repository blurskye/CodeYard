use super::{labels, pagination::Pagination, repository, Error, Result};
use crate::backend::{app::AppState, db, webhooks::payloads};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, MethodRouter},
};
use std::{collections::HashMap, sync::Arc};

type Params = HashMap<String, String>;

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let (offset, limit) = page.window()?;
    let users = db::assignees::list_assignable_users(&state.pool, repo.id, "", offset as i64, (limit + 1) as i64)
        .await
        .map_err(Error::unavailable)?;
    let next = users.len() > limit;
    let items = users.iter().take(limit).map(payloads::user).collect();
    page.response(&uri, items, next)
}

async fn check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    if let Some(number) = p.get("issue_number") {
        labels::item(&state.pool, repo.id, number).await?;
    }
    let user = db::users::accounts::get_user_by_username(&state.pool, &p["assignee"]).await.map_err(Error::unavailable)?.ok_or_else(Error::missing)?;
    if !db::assignees::assignable_user_ids(&state.pool, repo.id, &[user.id]).await.map_err(Error::unavailable)?.is_empty() {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::missing())
    }
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/list-assignees", get(list)),
        ("issues/check-user-can-be-assigned", get(check)),
        ("issues/check-user-can-be-assigned-to-issue", get(check)),
    ]
}
