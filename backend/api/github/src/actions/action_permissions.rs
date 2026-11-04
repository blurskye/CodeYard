//! The repository execution switch leaves native limits and engine selection intact.

use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::get;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

#[derive(Deserialize)]
struct RepositoryInput {
    enabled: bool,
    allowed_actions: Option<String>,
    sha_pinning_required: Option<bool>,
}

async fn get_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, actor) =
        codeyard_app::api_shared::github::repository(&state, &headers, &owner, &name, false)
            .await?;
    let actor = actor.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    if !codeyard_identity::repository_access::can_admin(&state.pool, &actor.user, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Repository administration required",
        ));
    }
    let settings = codeyard_db_actions::actions::get_repo_actions_settings(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?;
    Ok(Json(
        json!({"enabled":settings.enabled,"allowed_actions":"all","sha_pinning_required":false}),
    ))
}

async fn put_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<RepositoryInput>,
) -> Result<StatusCode> {
    let (repo, actor) =
        codeyard_app::api_shared::github::repository(&state, &headers, &owner, &name, true).await?;
    let actor = actor.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    if input
        .allowed_actions
        .as_deref()
        .is_some_and(|value| value != "all")
        || input.sha_pinning_required == Some(true)
    {
        return Err(Error::invalid(
            "Configure an available action execution policy",
        ));
    }
    codeyard_db_actions::actions::set_repo_enabled(
        &state.pool,
        actor.database_actor(),
        repo.id,
        input.enabled,
    )
    .await
    .map_err(crate::repos::settings_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "actions/get-github-actions-permissions-repository",
            get(get_repository),
        ),
        (
            "actions/set-github-actions-permissions-repository",
            put(put_repository),
        ),
    ]
}
