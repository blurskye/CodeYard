use crate::backend::api::github::pagination::Pagination;

use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;
use crate::backend::db::users::accounts::AccountCredential;

use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{delete, get, put, MethodRouter},
};
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("users/list-blocked-by-authenticated-user", get(list)),
        ("users/check-blocked", get(check)),
        ("users/block", put(block)),
        ("users/unblock", delete(unblock)),
    ]
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = crate::backend::api::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(crate::backend::api::github::auth::required)?;
    let (offset, limit) = page.window()?;
    let mut users = crate::backend::db::users::blocks::list_blocks(
        &state.pool,
        actor.user.id,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let more = users.len() > limit;
    users.truncate(limit);
    page.response(
        &uri,
        users
            .iter()
            .map(crate::backend::webhooks::payloads::user)
            .collect(),
        more,
    )
}
async fn check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> Result<StatusCode> {
    let actor = crate::backend::api::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(crate::backend::api::github::auth::required)?;
    let target = crate::backend::api::github::users::target(&state, &username).await?;
    if target.is_banned
        || !crate::backend::db::users::blocks::is_blocked(&state.pool, actor.user.id, target.id)
            .await
            .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn block(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> Result<StatusCode> {
    change(&state, &headers, &username, true).await
}
async fn unblock(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> Result<StatusCode> {
    change(&state, &headers, &username, false).await
}
async fn change(
    state: &AppState,
    headers: &HeaderMap,
    username: &str,
    blocked: bool,
) -> Result<StatusCode> {
    let actor = crate::backend::api::github::auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(crate::backend::api::github::auth::required)?;
    actor.require_write()?;
    crate::backend::identity::blocks::change(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        username,
        blocked,
    )
    .await
    .map_err(|error| match error {
        crate::backend::identity::blocks::BlockError::Missing => Error::missing(),
        crate::backend::identity::blocks::BlockError::SelfBlock => {
            Error::validation("User", "username", "invalid")
        }
        crate::backend::identity::blocks::BlockError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Account is restricted")
        }
        crate::backend::identity::blocks::BlockError::Unavailable => Error::unavailable(error),
    })?;
    Ok(StatusCode::NO_CONTENT)
}
