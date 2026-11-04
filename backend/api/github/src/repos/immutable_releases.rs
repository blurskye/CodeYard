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
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

fn error(error: anyhow::Error) -> Error {
    use codeyard_db_repos::repositories::immutable_releases::Refusal;
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
    let actor = actor.ok_or_else(codeyard_identity::github_auth::required)?;
    if !codeyard_repos_core::repositories::releases::immutable::enabled(
        &state.pool,
        Some(&actor.user),
        &repo,
    )
    .await
    .map_err(error)?
    {
        return Err(Error::missing());
    }
    let (_, enforced_by_owner) =
        codeyard_db_repos::repositories::immutable_releases::policy(&state.pool, repo.id)
            .await
            .map_err(error)?;
    Ok(Json(
        json!({"enabled":true,"enforced_by_owner":enforced_by_owner}),
    ))
}
async fn change(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    enabled: bool,
) -> Result<StatusCode> {
    let (repo, actor) = repository(state, headers, owner, name, true).await?;
    let actor = actor.ok_or_else(codeyard_identity::github_auth::required)?;
    codeyard_repos_core::repositories::releases::immutable::set_enabled(
        &state.pool,
        actor.database_actor(),
        repo.id,
        enabled,
    )
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
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/check-immutable-releases", get(check)),
        ("repos/enable-immutable-releases", put(enable)),
        ("repos/disable-immutable-releases", delete(disable)),
    ]
}
