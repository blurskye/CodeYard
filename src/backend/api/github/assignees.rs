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
    let names = db::assignees::list_assignable_usernames(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?;
    let (offset, limit) = page.window()?;
    let next = names.len() > offset + limit;
    let mut items = Vec::new();
    for name in names.into_iter().skip(offset).take(limit) {
        let user = db::users::accounts::get_user_by_username(&state.pool, &name)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(|| Error::unavailable(()))?;
        items.push(payloads::user(&user));
    }
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
    let names = db::assignees::list_assignable_usernames(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?;
    if names
        .iter()
        .any(|name| name.eq_ignore_ascii_case(&p["assignee"]))
    {
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
