//! Organization blocking is enforced by native repository and invitation authority.
use super::{auth, outcome, target, AppState, Error, Result};
use crate::backend::{
    api::github::pagination::Pagination,
    db::{
        self,
        organizations::{blocks, OrganizationOutcome},
    },
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{delete, get, put, MethodRouter},
};
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/list-blocked-users", get(list)),
        ("orgs/check-blocked-user", get(check)),
        ("orgs/block-user", put(block)),
        ("orgs/unblock-user", delete(unblock)),
    ]
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let (offset, limit) = page.window()?;
    let mut users = outcome(
        blocks::list(
            &state.pool,
            actor.user.id,
            org.id,
            0,
            (offset as i64, (limit + 1) as i64),
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    let more = users.len() > limit;
    users.truncate(limit);
    page.response(&uri, users.iter().map(payloads::user).collect(), more)
}
async fn check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
) -> Result<StatusCode> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let user = db::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    if !outcome(
        blocks::check(&state.pool, actor.user.id, org.id, user.id)
            .await
            .map_err(Error::unavailable)?,
    )? {
        return Err(Error::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn block(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
) -> Result<StatusCode> {
    change(&state, &headers, &name, &username, true).await
}
async fn unblock(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
) -> Result<StatusCode> {
    change(&state, &headers, &name, &username, false).await
}
async fn change(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    username: &str,
    blocked: bool,
) -> Result<StatusCode> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(state, name).await?;
    let user = db::users::accounts::get_user_by_username(&state.pool, username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let result = blocks::change(&state.pool, actor.database_actor(), org.id, user.id, blocked)
        .await
        .map_err(Error::unavailable)?;
    if matches!(result, OrganizationOutcome::Conflict) {
        return Err(Error::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Remove the user from the organization before blocking them",
        ));
    }
    outcome(result)?;
    Ok(StatusCode::NO_CONTENT)
}
