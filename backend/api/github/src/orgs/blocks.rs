//! Organization blocking is enforced by native repository and invitation authority.
use crate::orgs::outcome;
use crate::orgs::target;
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
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
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
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    let org = target(&state, &name).await?;
    let (offset, limit) = page.window()?;
    let mut users = outcome(
        codeyard_db_orgs::organizations::blocks::list(
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
    Path((name, username)): Path<(String, String)>,
) -> Result<StatusCode> {
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    let org = target(&state, &name).await?;
    let user = codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    if !outcome(
        codeyard_db_orgs::organizations::blocks::check(&state.pool, actor.user.id, org.id, user.id)
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
    let actor = codeyard_identity::github_auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    actor.require_write()?;
    let org = target(state, name).await?;
    let user = codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let result = codeyard_db_orgs::organizations::blocks::change(
        &state.pool,
        actor.database_actor(),
        org.id,
        user.id,
        blocked,
    )
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
