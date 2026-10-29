//! Repository cache policies are administrative changes to the runtime's existing limits.
use crate::backend::{
    api::github::{auth, repository, Error, Result},
    app::AppState,
    db::{
        self,
        actions::{CacheLimit, CachePolicyError},
    },
    repositories::access,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

async fn get_policy(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    kind: CacheLimit,
) -> Result<Json<Value>> {
    let (repo, authentication) = repository(state, headers, owner, name, false).await?;
    let actor = authentication.ok_or_else(auth::required)?.user;
    if !access::can_admin(&state.pool, &actor, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Repository administration denied",
        ));
    }
    let site = db::actions::get_actions_settings(&state.pool)
        .await
        .map_err(Error::unavailable)?;
    let own = db::actions::get_repo_actions_settings(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?;
    Ok(Json(match kind {
        CacheLimit::Storage => {
            json!({"max_cache_size_gb":site.cache_size_bytes(&own)/(1024*1024*1024)})
        }
        CacheLimit::Retention => json!({"max_cache_retention_days":site.cache_expiry_days(&own)}),
    }))
}

#[derive(Deserialize)]
pub(super) struct Input {
    max_cache_size_gb: Option<i32>,
    max_cache_retention_days: Option<i32>,
}

impl Input {
    pub(super) fn value(self, kind: CacheLimit) -> Result<i32> {
        match kind {
            CacheLimit::Storage => self.max_cache_size_gb,
            CacheLimit::Retention => self.max_cache_retention_days,
        }
        .ok_or_else(|| Error::new(StatusCode::BAD_REQUEST, "Cache limit is required"))
    }
}

async fn set_policy(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    kind: CacheLimit,
    input: Input,
) -> Result<StatusCode> {
    let (repo, authentication) = repository(state, headers, owner, name, true).await?;
    let actor = authentication.ok_or_else(auth::required)?.user.id;
    let value = input.value(kind)?;
    db::actions::set_cache_limit(&state.pool, actor, repo.id, kind, value)
        .await
        .map_err(|error| match error.downcast_ref::<CachePolicyError>() {
            Some(CachePolicyError::Denied) => {
                Error::new(StatusCode::FORBIDDEN, "Repository administration denied")
            }
            Some(CachePolicyError::Limit) => Error::new(
                StatusCode::BAD_REQUEST,
                "Cache limit must be positive and no greater than the site or organization limit",
            ),
            None => Error::unavailable(error),
        })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_storage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    get_policy(&state, &headers, &owner, &name, CacheLimit::Storage).await
}
async fn get_retention(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    get_policy(&state, &headers, &owner, &name, CacheLimit::Retention).await
}
async fn set_storage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Input>,
) -> Result<StatusCode> {
    set_policy(&state, &headers, &owner, &name, CacheLimit::Storage, input).await
}
async fn set_retention(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Input>,
) -> Result<StatusCode> {
    set_policy(
        &state,
        &headers,
        &owner,
        &name,
        CacheLimit::Retention,
        input,
    )
    .await
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "actions/get-actions-cache-storage-limit-for-repository",
            get(get_storage),
        ),
        (
            "actions/get-actions-cache-retention-limit-for-repository",
            get(get_retention),
        ),
        (
            "actions/set-actions-cache-storage-limit-for-repository",
            put(set_storage),
        ),
        (
            "actions/set-actions-cache-retention-limit-for-repository",
            put(set_retention),
        ),
    ]
}
