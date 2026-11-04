use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use std::collections::HashMap;
use std::sync::Arc;

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
    let users = codeyard_db_repos::assignees::list_assignable_users(
        &state.pool,
        repo.id,
        "",
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let next = users.len() > limit;
    let items = users
        .iter()
        .take(limit)
        .map(codeyard_app::webhooks::payloads::user)
        .collect();
    page.response(&uri, items, next)
}

async fn check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    if let Some(number) = p.get("issue_number") {
        codeyard_app::api_shared::github::labels::item(&state.pool, repo.id, number).await?;
    }
    let user =
        codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, &p["assignee"])
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    if !codeyard_db_repos::assignees::assignable_user_ids(&state.pool, repo.id, &[user.id])
        .await
        .map_err(Error::unavailable)?
        .is_empty()
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::missing())
    }
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/list-assignees", get(list)),
        ("issues/check-user-can-be-assigned", get(check)),
        ("issues/check-user-can-be-assigned-to-issue", get(check)),
    ]
}
