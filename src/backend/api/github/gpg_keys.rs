use super::{auth, pagination::Pagination, users, Error, Result};
use crate::backend::app::AppState;
use crate::backend::db::users::accounts::AccountCredential;
use crate::backend::db::users::gpg_keys;
use crate::backend::db::users::gpg_keys::GpgKey;
use crate::backend::identity::gpg;
use crate::backend::identity::gpg::GpgError;
use crate::backend::webhooks::payloads;
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{delete, get, post, MethodRouter},
    Json,
};
use codeyard_db_core::models::User;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("users/list-gpg-keys-for-authenticated-user", get(list)),
        ("users/list-gpg-keys-for-user", get(public)),
        ("users/get-gpg-key-for-authenticated-user", get(detail)),
        ("users/create-gpg-key-for-authenticated-user", post(create)),
        (
            "users/delete-gpg-key-for-authenticated-user",
            delete(remove),
        ),
    ]
}
async fn documents(state: &AppState, user: &User, keys: &[GpgKey]) -> Result<Vec<Value>> {
    let verified: Vec<String> = crate::backend::identity::signatures::addresses(&state.pool, user)
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .filter_map(|(email, verified)| verified.then_some(email))
        .collect();
    let subkeys = gpg_keys::list_subkeys(
        &state.pool,
        user.id,
        &keys.iter().map(|key| key.id).collect::<Vec<_>>(),
    )
    .await
    .map_err(Error::unavailable)?;
    Ok(keys
        .iter()
        .map(|key| payloads::gpg_key(key, &subkeys, &verified))
        .collect())
}
async fn page(
    state: &AppState,
    user: &User,
    page: Pagination,
    uri: axum::http::Uri,
) -> Result<Response> {
    let (offset, limit) = page.window()?;
    let mut keys = gpg_keys::list_gpg_keys(&state.pool, user.id, (limit + 1) as i64, offset as i64)
        .await
        .map_err(Error::unavailable)?;
    let more = keys.len() > limit;
    keys.truncate(limit);
    page.response(&uri, documents(state, user, &keys).await?, more)
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(pagination): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    page(&state, &actor.user, pagination, uri).await
}
async fn public(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(pagination): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    page(
        &state,
        &users::target(&state, &username).await?,
        pagination,
        uri,
    )
    .await
}
fn failure(error: GpgError) -> Error {
    match error {
        GpgError::Missing => Error::missing(),
        GpgError::Denied => Error::new(StatusCode::FORBIDDEN, "Account is restricted"),
        GpgError::Unavailable => Error::unavailable(error),
        GpgError::AlreadyUsed => Error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Validation Failed".into(),
            Some(
                serde_json::json!([{"resource":"GpgKey","field":"key_id","code":"already_exists","message":"key_id already exists"}]),
            ),
        ),
        GpgError::InvalidName => Error::validation("GpgKey", "name", "invalid"),
        _ => Error::validation("GpgKey", "armored_public_key", "invalid"),
    }
}
#[derive(Deserialize)]
struct CreateKey {
    name: Option<String>,
    armored_public_key: String,
}
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    input: std::result::Result<Json<CreateKey>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Value>)> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let Json(input) =
        input.map_err(|_| Error::validation("GpgKey", "armored_public_key", "invalid"))?;
    let key = gpg::enroll(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        input.name.as_deref(),
        &input.armored_public_key,
    )
    .await
    .map_err(failure)?;
    Ok((
        StatusCode::CREATED,
        Json(documents(&state, &actor.user, &[key]).await?.remove(0)),
    ))
}
async fn detail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let key = gpg_keys::get_gpg_key(&state.pool, actor.user.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(Json(
        documents(&state, &actor.user, &[key]).await?.remove(0),
    ))
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
    gpg::delete(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        id,
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
