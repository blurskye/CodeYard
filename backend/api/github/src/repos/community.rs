use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_git_storage::api_shared::github::contents::store_error;
use codeyard_types::github_error::Result;
use serde_json::Value;
use std::sync::Arc;

async fn get_profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let profile = codeyard_repositories::repositories::community::load(
        &state.pool,
        &repo,
        &owner,
        state.cluster.as_ref(),
    )
    .await
    .map_err(store_error)?;
    Ok(Json(
        codeyard_repositories::webhook_payloads::repository::community_profile(&profile),
    ))
}
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("repos/get-community-profile-metrics", get(get_profile))]
}
