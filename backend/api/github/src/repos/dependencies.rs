//! Native leased dependency scans and ordinary, reviewed security-update pull requests.

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
use codeyard_app::repositories::dependencies::DependencyError;
use codeyard_app::repositories::dependencies::Status;
use codeyard_db_repos::repositories::dependencies::Feature;
use codeyard_git::error::ErrorKind;
use codeyard_identity::repository_access::AccessError;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use std::sync::Arc;

fn failure(error: DependencyError) -> Error {
    match error {
        DependencyError::Access(AccessError::NotFound) => Error::missing(),
        DependencyError::Access(AccessError::Unavailable) => Error::unavailable(()),
        DependencyError::Access(_) => {
            Error::new(StatusCode::FORBIDDEN, "Repository administration required")
        }
        DependencyError::Store(error) if error.is_write_denied() => {
            Error::new(StatusCode::FORBIDDEN, "Repository administration required")
        }
        DependencyError::Store(error) => match error.kind {
            ErrorKind::NotFound | ErrorKind::MissingPath => Error::missing(),
            ErrorKind::Invalid => Error::invalid(error.message),
            ErrorKind::Conflict => Error::new(StatusCode::CONFLICT, error.message),
            ErrorKind::Unavailable => Error::unavailable(()),
        },
    }
}
async fn status(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    feature: Feature,
) -> Result<Status> {
    let (repo, actor) = repository(state, headers, owner, name, false).await?;
    let actor = actor.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    codeyard_app::repositories::dependencies::status(&state.pool, actor.user.id, repo.id, feature)
        .await
        .map_err(failure)
}
async fn change(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    feature: Feature,
    enabled: bool,
) -> Result<StatusCode> {
    let (repo, actor) = repository(state, headers, owner, name, true).await?;
    let actor = actor.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    codeyard_app::repositories::dependencies::set_enabled(
        &state.pool,
        state.cluster.as_ref(),
        actor.user.id,
        codeyard_db_core::queries::users::accounts::OwnedAccountCredential::Token(
            actor.token.id,
            actor.token.token_hash.clone(),
        ),
        repo.id,
        feature,
        enabled,
        true, // This explicit REST operation authorizes private dependency export.
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn check_updates(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Status>> {
    Ok(Json(
        status(&state, &headers, &owner, &name, Feature::Updates).await?,
    ))
}
async fn check_alerts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    if status(&state, &headers, &owner, &name, Feature::Alerts)
        .await?
        .enabled
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::missing())
    }
}
async fn enable_alerts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    change(&state, &headers, &owner, &name, Feature::Alerts, true).await
}
async fn disable_alerts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    change(&state, &headers, &owner, &name, Feature::Alerts, false).await
}
async fn enable_updates(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    change(&state, &headers, &owner, &name, Feature::Updates, true).await
}
async fn disable_updates(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    change(&state, &headers, &owner, &name, Feature::Updates, false).await
}
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/check-vulnerability-alerts", get(check_alerts)),
        ("repos/enable-vulnerability-alerts", put(enable_alerts)),
        ("repos/disable-vulnerability-alerts", delete(disable_alerts)),
        ("repos/check-automated-security-fixes", get(check_updates)),
        ("repos/enable-automated-security-fixes", put(enable_updates)),
        (
            "repos/disable-automated-security-fixes",
            delete(disable_updates),
        ),
    ]
}
