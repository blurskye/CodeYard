use super::{api_url, auth, pagination::Pagination, Error, Result};
use crate::backend::{
    app::AppState,
    db::users::{accounts::AccountCredential, signing_keys},
    identity::keys::{self, KeyError},
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "users/list-ssh-signing-keys-for-authenticated-user",
            get(own),
        ),
        ("users/list-ssh-signing-keys-for-user", get(public)),
        (
            "users/get-ssh-signing-key-for-authenticated-user",
            get(get_key),
        ),
        (
            "users/create-ssh-signing-key-for-authenticated-user",
            post(create),
        ),
        (
            "users/delete-ssh-signing-key-for-authenticated-user",
            delete(remove),
        ),
    ]
}
async fn own(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    list(&state, actor.user.id, page, uri).await
}
async fn public(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    let user = super::users::target(&state, &username).await?;
    list(&state, user.id, page, uri).await
}
async fn list(
    state: &AppState,
    user: i64,
    page: Pagination,
    uri: axum::http::Uri,
) -> Result<Response> {
    let (offset, limit) = page.window()?;
    let keys =
        signing_keys::list_signing_keys(&state.pool, user, (limit + 1) as i64, offset as i64)
            .await
            .map_err(Error::unavailable)?;
    let more = keys.len() > limit;
    page.response(&uri, keys.into_iter().map(|key| json!(key)).collect(), more)
}
async fn get_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<signing_keys::SigningKey>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    Ok(Json(
        signing_keys::get_signing_key(&state.pool, actor.user.id, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?,
    ))
}
fn failure(error: KeyError) -> Error {
    match error {
        KeyError::InvalidName => Error::validation("Key", "title", "invalid"),
        KeyError::InvalidKey => Error::validation("Key", "key", "invalid"),
        KeyError::AlreadyUsed => Error::validation("Key", "key", "already_exists"),
        KeyError::Missing => Error::missing(),
        KeyError::AccountRestricted | KeyError::InvalidSession => {
            Error::new(StatusCode::FORBIDDEN, "Account is restricted")
        }
        KeyError::Unavailable => Error::unavailable(error),
    }
}
#[derive(Deserialize)]
struct Input {
    key: String,
    title: Option<String>,
}
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    input: std::result::Result<Json<Input>, axum::extract::rejection::JsonRejection>,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let Json(input) = input.map_err(|_| Error::validation("Key", "key", "invalid"))?;
    let key = keys::enroll_signing(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id),
        input
            .title
            .as_deref()
            .filter(|title| !title.is_empty())
            .unwrap_or("SSH signing key"),
        &input.key,
    )
    .await
    .map_err(failure)?;
    Ok((
        StatusCode::CREATED,
        [(
            header::LOCATION,
            api_url(&format!("/user/ssh_signing_keys/{}", key.id)),
        )],
        Json(key),
    )
        .into_response())
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    keys::delete_signing(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id),
        id,
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
