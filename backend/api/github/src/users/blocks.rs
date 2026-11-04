use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::put;
use axum::routing::MethodRouter;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use std::sync::Arc;

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
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
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    let (offset, limit) = page.window()?;
    let mut users = codeyard_db_identity::users::blocks::list_blocks(
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
            .map(codeyard_webhooks::payloads::user)
            .collect(),
        more,
    )
}
async fn check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> Result<StatusCode> {
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    let target = crate::users::target(&state, &username).await?;
    if target.is_banned
        || !codeyard_db_identity::users::blocks::is_blocked(&state.pool, actor.user.id, target.id)
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
    let actor = codeyard_identity::github_auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    actor.require_write()?;
    codeyard_identity::blocks::change(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        username,
        blocked,
    )
    .await
    .map_err(|error| match error {
        codeyard_identity::blocks::BlockError::Missing => Error::missing(),
        codeyard_identity::blocks::BlockError::SelfBlock => {
            Error::validation("User", "username", "invalid")
        }
        codeyard_identity::blocks::BlockError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Account is restricted")
        }
        codeyard_identity::blocks::BlockError::Unavailable => Error::unavailable(error),
    })?;
    Ok(StatusCode::NO_CONTENT)
}
