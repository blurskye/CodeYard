use super::{auth, repository, Error, Result};
use crate::backend::{app::AppState, db, repositories::releases::immutable};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, put, MethodRouter},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;
fn error(error: anyhow::Error) -> Error {
    use db::repositories::immutable_releases::Refusal;
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Repository administration required")
        }
        Some(Refusal::Archived) => Error::new(
            StatusCode::CONFLICT,
            "Archived repositories cannot change immutable release settings",
        ),
        None => Error::unavailable(error),
    }
}
async fn check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = actor.ok_or_else(auth::required)?;
    if !immutable::enabled(&state.pool, Some(&actor.user), &repo)
        .await
        .map_err(error)?
    {
        return Err(Error::missing());
    }
    let (_, enforced_by_owner) = db::repositories::immutable_releases::policy(&state.pool, repo.id).await.map_err(error)?;
    Ok(Json(json!({"enabled":true,"enforced_by_owner":enforced_by_owner})))
}
async fn change(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    enabled: bool,
) -> Result<StatusCode> {
    let (repo, actor) = repository(state, headers, owner, name, true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    immutable::set_enabled(&state.pool, actor.database_actor(), repo.id, enabled)
        .await
        .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn enable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    change(&state, &headers, &owner, &name, true).await
}
async fn disable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    change(&state, &headers, &owner, &name, false).await
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/check-immutable-releases", get(check)),
        ("repos/enable-immutable-releases", put(enable)),
        ("repos/disable-immutable-releases", delete(disable)),
    ]
}
