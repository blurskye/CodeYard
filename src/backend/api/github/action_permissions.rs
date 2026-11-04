//! The repository execution switch leaves native limits and engine selection intact.

use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
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
        crate::backend::api::github::repository(&state, &headers, &owner, &name, false).await?;
    let actor = actor.ok_or_else(crate::backend::api::github::auth::required)?;
    if !crate::backend::repositories::access::can_admin(&state.pool, &actor.user, &repo)
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
        crate::backend::api::github::repository(&state, &headers, &owner, &name, true).await?;
    let actor = actor.ok_or_else(crate::backend::api::github::auth::required)?;
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
    .map_err(crate::backend::api::github::repos::settings_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
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
