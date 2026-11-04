//! Private vulnerability reporting is backed by the native private-report workflow.

use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, put, MethodRouter},
    Json,
};
use std::sync::Arc;

fn failure(error: anyhow::Error) -> Error {
    match error.downcast_ref::<codeyard_model::security_reports::Refusal>() {
        Some(codeyard_model::security_reports::Refusal::Denied) => Error::new(
            StatusCode::FORBIDDEN,
            "Private vulnerability reporting mutation denied",
        ),
        Some(codeyard_model::security_reports::Refusal::Missing) => Error::missing(),
        Some(codeyard_model::security_reports::Refusal::Invalid(message)) => {
            Error::invalid(*message)
        }
        Some(codeyard_model::security_reports::Refusal::Limit) => Error::new(
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
) -> Result<Json<serde_json::Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let enabled = codeyard_db_repos::repositories::security::enabled(&state.pool, repo.id)
        .await
        .map_err(failure)?;
    Ok(Json(serde_json::json!({"enabled": enabled})))
}
async fn set(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    enabled: bool,
) -> Result<StatusCode> {
    let (repo, actor) = repository(state, headers, owner, name, true).await?;
    let actor = actor.ok_or_else(crate::backend::api::github::auth::required)?;
    codeyard_db_repos::repositories::security::set_enabled(
        &state.pool,
        actor.database_actor(),
        repo.id,
        enabled,
    )
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
