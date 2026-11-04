use crate::backend::api::github::contents::store_error;
use crate::backend::api::github::repository;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, MethodRouter},
    Json,
};
use serde_json::Value;
use std::sync::Arc;
async fn get_profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let profile = crate::backend::repositories::community::load(
        &state.pool,
        &repo,
        &owner,
        state.cluster.as_ref(),
    )
    .await
    .map_err(store_error)?;
    Ok(Json(crate::backend::webhooks::payloads::community_profile(
        &profile,
    )))
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("repos/get-community-profile-metrics", get(get_profile))]
}
