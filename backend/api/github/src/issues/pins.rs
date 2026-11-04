use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::delete;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<codeyard_db_repos::pins::PinRefusal>() {
        Some(codeyard_db_repos::pins::PinRefusal::Denied) => Error::new(
            StatusCode::FORBIDDEN,
            "Pinning comments requires write access",
        ),
        Some(codeyard_db_repos::pins::PinRefusal::Missing) => Error::missing(),
        Some(codeyard_db_repos::pins::PinRefusal::Occupied) => {
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
    codeyard_webhooks::comment_documents::Comment,
)> {
    let (repo, auth) = repository(state, headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    let id = p["comment_id"].parse().map_err(|_| Error::missing())?;
    let comment =
        codeyard_webhooks::comment_documents::find(&state.pool, repo.id, id).await?;
    codeyard_db_repos::pins::set_pin(
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
        codeyard_webhooks::comment_documents::document(
            &state.pool,
            &repo,
            &p["owner"],
            &comment,
        )
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
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/pin-comment", put(pin)),
        ("issues/unpin-comment", delete(unpin)),
    ]
}
