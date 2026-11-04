use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, put, MethodRouter},
};
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

#[derive(Default, Deserialize)]
struct Lock {
    lock_reason: Option<String>,
}

pub(in crate::backend::api) fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<crate::backend::db::discussions::LockRefusal>() {
        Some(crate::backend::db::discussions::LockRefusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Discussion lock denied")
        }
        Some(crate::backend::db::discussions::LockRefusal::Missing) => Error::missing(),
        Some(crate::backend::db::discussions::LockRefusal::InvalidReason) => {
            Error::validation("Issue", "lock_reason", "invalid")
        }
        None => Error::unavailable(error),
    }
}

async fn change(
    state: Arc<AppState>,
    headers: HeaderMap,
    p: HashMap<String, String>,
    locked: bool,
    reason: Option<&str>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth
        .ok_or_else(crate::backend::api::github::auth::required)?
        .database_actor();
    let item =
        crate::backend::api::github::labels::item(&state.pool, repo.id, &p["issue_number"]).await?;
    crate::backend::db::discussions::set_lock(&state.pool, actor, repo.id, item, locked, reason)
        .await
        .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn lock(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
    body: Bytes,
) -> Result<StatusCode> {
    let input: Lock = crate::backend::api::github::optional_json::decode(&body)
        .map_err(|_| Error::validation("Issue", "lock_reason", "invalid"))?
        .unwrap_or_default();
    change(state, headers, p, true, input.lock_reason.as_deref()).await
}
async fn unlock(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
) -> Result<StatusCode> {
    change(state, headers, p, false, None).await
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/lock", put(lock)),
        ("issues/unlock", delete(unlock)),
    ]
}
