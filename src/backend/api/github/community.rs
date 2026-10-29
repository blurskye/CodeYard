use super::{contents::store_error, repository, Result};
use crate::backend::{app::AppState, repositories::community, webhooks::payloads};
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
    let profile = community::load(&state.pool, &repo, &owner, state.cluster.as_ref())
        .await
        .map_err(store_error)?;
    Ok(Json(payloads::community_profile(&profile)))
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("repos/get-community-profile-metrics", get(get_profile))]
}
