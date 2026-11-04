use crate::backend::api::github::pagination::Pagination;
use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use codeyard_db_repos::repositories::autolinks::NewAutolink;
use codeyard_db_repos::repositories::autolinks::Refusal;

use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
fn failure(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Repository administration required")
        }
        Some(Refusal::Missing) => Error::missing(),
        Some(refusal) => Error::invalid(refusal.to_string()),
        None => Error::unavailable(error),
    }
}
fn yes() -> bool {
    true
}
#[derive(Deserialize)]
struct Create {
    key_prefix: String,
    url_template: String,
    #[serde(default = "yes")]
    is_alphanumeric: bool,
}
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Create>,
) -> Result<Response> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = actor.ok_or_else(crate::backend::api::github::auth::required)?;
    let input = NewAutolink::new(
        &input.key_prefix,
        &input.url_template,
        input.is_alphanumeric,
    )
    .map_err(|error| Error::invalid(error.to_string()))?;
    let row = codeyard_db_repos::repositories::autolinks::create(
        &state.pool,
        actor.database_actor(),
        repo.id,
        &input,
    )
    .await
    .map_err(failure)?;
    Ok((
        StatusCode::CREATED,
        Json(crate::backend::webhooks::payloads::autolink(&row)),
    )
        .into_response())
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = actor
        .ok_or_else(crate::backend::api::github::auth::required)?
        .user;
    let (offset, limit) = page.window()?;
    let rows = codeyard_db_repos::repositories::autolinks::list(
        &state.pool,
        actor.id,
        repo.id,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(failure)?;
    let more = rows.len() > limit;
    page.response(
        &uri,
        rows.iter()
            .map(crate::backend::webhooks::payloads::autolink)
            .collect(),
        more,
    )
}
async fn get_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = actor
        .ok_or_else(crate::backend::api::github::auth::required)?
        .user;
    Ok(Json(crate::backend::webhooks::payloads::autolink(
        &codeyard_db_repos::repositories::autolinks::get(&state.pool, actor.id, repo.id, id)
            .await
            .map_err(failure)?,
    )))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = actor.ok_or_else(crate::backend::api::github::auth::required)?;
    codeyard_db_repos::repositories::autolinks::remove(
        &state.pool,
        actor.database_actor(),
        repo.id,
        id,
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-autolinks", get(list)),
        ("repos/get-autolink", get(get_one)),
        ("repos/create-autolink", post(create)),
        ("repos/delete-autolink", delete(remove)),
    ]
}
