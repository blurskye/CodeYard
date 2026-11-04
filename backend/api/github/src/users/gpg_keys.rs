use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_core::models::User;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_db_identity::users::gpg_keys::GpgKey;
use codeyard_identity::gpg::GpgError;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
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
    let verified: Vec<String> = codeyard_identity::signatures::addresses(&state.pool, user)
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .filter_map(|(email, verified)| verified.then_some(email))
        .collect();
    let subkeys = codeyard_db_identity::users::gpg_keys::list_subkeys(
        &state.pool,
        user.id,
        &keys.iter().map(|key| key.id).collect::<Vec<_>>(),
    )
    .await
    .map_err(Error::unavailable)?;
    Ok(keys
        .iter()
        .map(|key| codeyard_webhooks::payloads::gpg_key(key, &subkeys, &verified))
        .collect())
}
async fn page(
    state: &AppState,
    user: &User,
    page: Pagination,
    uri: axum::http::Uri,
) -> Result<Response> {
    let (offset, limit) = page.window()?;
    let mut keys = codeyard_db_identity::users::gpg_keys::list_gpg_keys(
        &state.pool,
        user.id,
        (limit + 1) as i64,
        offset as i64,
    )
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
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    page(&state, &actor.user, pagination, uri).await
}
async fn public(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(pagination): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    codeyard_identity::github_auth::authenticate(&state, &headers, false).await?;
    page(
        &state,
        &crate::users::target(&state, &username).await?,
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
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    actor.require_write()?;
    let Json(input) =
        input.map_err(|_| Error::validation("GpgKey", "armored_public_key", "invalid"))?;
    let key = codeyard_identity::gpg::enroll(
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
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    let key = codeyard_db_identity::users::gpg_keys::get_gpg_key(&state.pool, actor.user.id, id)
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
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    actor.require_write()?;
    codeyard_identity::gpg::delete(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        id,
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
