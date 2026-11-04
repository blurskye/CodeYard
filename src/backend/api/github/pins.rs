use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, put, MethodRouter},
    Json,
};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<crate::backend::db::pins::PinRefusal>() {
        Some(crate::backend::db::pins::PinRefusal::Denied) => Error::new(
            StatusCode::FORBIDDEN,
            "Pinning comments requires write access",
        ),
        Some(crate::backend::db::pins::PinRefusal::Missing) => Error::missing(),
        Some(crate::backend::db::pins::PinRefusal::Occupied) => {
            Error::invalid("Unpin the current comment before pinning another")
        }
        None => Error::unavailable(error),
    }
}
async fn change(
    state: &AppState,
    headers: &HeaderMap,
    p: &HashMap<String, String>,
    pinned: bool,
) -> Result<(
    codeyard_db_core::models::Repository,
    crate::backend::api::github::comments::Comment,
)> {
    let (repo, auth) = repository(state, headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(crate::backend::api::github::auth::required)?;
    let id = p["comment_id"].parse().map_err(|_| Error::missing())?;
    let comment = crate::backend::api::github::comments::find(&state.pool, repo.id, id).await?;
    crate::backend::db::pins::set_pin(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        repo.id,
        comment.item,
        comment.id,
        pinned,
    )
    .await
    .map_err(error)?;
    Ok((repo, comment))
}
async fn pin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let (repo, comment) = change(&state, &headers, &p, true).await?;
    Ok(Json(
        crate::backend::api::github::comments::document(&state.pool, &repo, &p["owner"], &comment)
            .await?,
    ))
}
async fn unpin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
) -> Result<StatusCode> {
    change(&state, &headers, &p, false).await?;
    Ok(StatusCode::NO_CONTENT)
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/pin-comment", put(pin)),
        ("issues/unpin-comment", delete(unpin)),
    ]
}
