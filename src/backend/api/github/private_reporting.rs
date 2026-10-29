//! Private vulnerability reporting is backed by the native private-report workflow.
use super::{auth, repository, Error, Result};
use crate::backend::{app::AppState, db::repositories::security};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, put, MethodRouter},
};
use std::sync::Arc;

fn failure(error: anyhow::Error) -> Error {
    match error.downcast_ref::<security::Refusal>() {
        Some(security::Refusal::Denied) => Error::new(
            StatusCode::FORBIDDEN,
            "Private vulnerability reporting mutation denied",
        ),
        Some(security::Refusal::Missing) => Error::missing(),
        Some(security::Refusal::Invalid(message)) => Error::invalid(*message),
        Some(security::Refusal::Limit) => Error::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Security reporting limit reached",
        ),
        None => Error::unavailable(error),
    }
}
async fn check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    if security::enabled(&state.pool, repo.id)
        .await
        .map_err(failure)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::missing())
    }
}
async fn set(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    enabled: bool,
) -> Result<StatusCode> {
    let (repo, actor) = repository(state, headers, owner, name, true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    security::set_enabled(&state.pool, actor.user.id, repo.id, enabled)
        .await
        .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn enable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    set(&state, &headers, &owner, &name, true).await
}
async fn disable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    set(&state, &headers, &owner, &name, false).await
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/check-private-vulnerability-reporting", get(check)),
        ("repos/enable-private-vulnerability-reporting", put(enable)),
        (
            "repos/disable-private-vulnerability-reporting",
            delete(disable),
        ),
    ]
}
