//! Private vulnerability reporting is backed by the native private-report workflow.

use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
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
    let actor = actor.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
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
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/check-private-vulnerability-reporting", get(check)),
        ("repos/enable-private-vulnerability-reporting", put(enable)),
        (
            "repos/disable-private-vulnerability-reporting",
            delete(disable),
        ),
    ]
}
