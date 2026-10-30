//! Organization ceilings are enforced by the same cache transactions as repository limits.
use super::cache_policy::Input;
use crate::backend::{
    api::github::{auth, Error, Result},
    app::AppState,
    db::{
        self,
        actions::{CacheLimit, CachePolicyError},
        organizations::OrganizationOutcome,
    },
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, put, MethodRouter},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

fn outcome<T>(value: OrganizationOutcome<T>) -> Result<T> {
    match value {
        OrganizationOutcome::Conflict => Err(Error::new(
            StatusCode::CONFLICT,
            "Organization policy conflict",
        )),
        OrganizationOutcome::Denied => Err(Error::new(
            StatusCode::FORBIDDEN,
            "Organization administration required",
        )),
        other => crate::backend::api::github::orgs::outcome(other),
    }
}

async fn get_policy(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    kind: CacheLimit,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?
        .user
        .id;
    let own = outcome(
        db::actions::get_org_cache_settings(&state.pool, actor, name)
            .await
            .map_err(Error::unavailable)?,
    )?;
    let site = db::actions::get_actions_settings(&state.pool)
        .await
        .map_err(Error::unavailable)?;
    Ok(Json(match kind {
        CacheLimit::Storage => {
            json!({"max_cache_size_gb":kind.effective(site.cache_size_gib, own.cache_size_gib, None)})
        }
        CacheLimit::Retention => {
            json!({"max_cache_retention_days":kind.effective(site.cache_retention_days, own.cache_retention_days, None)})
        }
    }))
}

async fn set_policy(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    kind: CacheLimit,
    input: Input,
) -> Result<StatusCode> {
    let authentication = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    authentication.require_write()?;
    let value = input.value(kind)?;
    outcome(
        db::actions::set_org_cache_limit(&state.pool, authentication.user.id, name, kind, value)
            .await
            .map_err(|error| match error.downcast_ref::<CachePolicyError>() {
                Some(CachePolicyError::Limit) => Error::new(
                    StatusCode::BAD_REQUEST,
                    "Cache limit must be positive and no greater than the site limit",
                ),
                _ => Error::unavailable(error),
            })?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_storage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    get_policy(&state, &headers, &name, CacheLimit::Storage).await
}
async fn get_retention(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    get_policy(&state, &headers, &name, CacheLimit::Retention).await
}
async fn set_storage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(input): Json<Input>,
) -> Result<StatusCode> {
    set_policy(&state, &headers, &name, CacheLimit::Storage, input).await
}
async fn set_retention(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(input): Json<Input>,
) -> Result<StatusCode> {
    set_policy(&state, &headers, &name, CacheLimit::Retention, input).await
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "actions/get-actions-cache-storage-limit-for-organization",
            get(get_storage),
        ),
        (
            "actions/get-actions-cache-retention-limit-for-organization",
            get(get_retention),
        ),
        (
            "actions/set-actions-cache-storage-limit-for-organization",
            put(set_storage),
        ),
        (
            "actions/set-actions-cache-retention-limit-for-organization",
            put(set_retention),
        ),
    ]
}
