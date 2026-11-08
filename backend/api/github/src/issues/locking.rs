use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::delete;
use axum::routing::put;
use axum::routing::MethodRouter;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default, Deserialize)]
struct Lock {
    lock_reason: Option<String>,
}

pub fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<codeyard_db_repos::discussions::LockRefusal>() {
        Some(codeyard_db_repos::discussions::LockRefusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Discussion lock denied")
        }
        Some(codeyard_db_repos::discussions::LockRefusal::Missing) => Error::missing(),
        Some(codeyard_db_repos::discussions::LockRefusal::InvalidReason) => {
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
        .ok_or_else(codeyard_identity::github_auth::required)?
        .database_actor();
    let item =
        codeyard_app::api_shared::github::labels::item(&state.pool, repo.id, &p["issue_number"])
            .await?;
    codeyard_db_repos::discussions::set_lock(&state.pool, actor, repo.id, item, locked, reason)
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
    let input: Lock = crate::optional_json::decode(&body)
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
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/lock", put(lock)),
        ("issues/unlock", delete(unlock)),
    ]
}
