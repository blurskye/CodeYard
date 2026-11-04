//! Organization ceilings are enforced by the same cache transactions as repository limits.
use crate::actions::cache_policy::Input;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::get;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::actions::CacheLimit;
use codeyard_db_core::queries::actions::CachePolicyError;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::json;
use serde_json::Value;
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
        other => crate::orgs::outcome(other),
    }
}

async fn get_policy(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    kind: CacheLimit,
) -> Result<Json<Value>> {
    let actor = codeyard_identity::github_auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?
        .user
        .id;
    let own = outcome(
        codeyard_db_actions::actions::get_org_cache_settings(&state.pool, actor, name)
            .await
            .map_err(Error::unavailable)?,
    )?;
    let site = codeyard_db_actions::actions::get_actions_settings(&state.pool)
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
    let authentication = codeyard_identity::github_auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    authentication.require_write()?;
    let value = input.value(kind)?;
    outcome(
        codeyard_db_actions::actions::set_org_cache_limit(
            &state.pool,
            authentication.user.id,
            name,
            kind,
            value,
        )
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
