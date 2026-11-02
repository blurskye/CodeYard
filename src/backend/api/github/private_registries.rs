use super::{action_secrets, dependabot, pagination::Pagination, Error, Result};
use crate::backend::{
    app::AppState,
    db::{self, private_registries::RegistryError},
};
use axum::{
    extract::{rejection::JsonRejection, OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, MethodRouter},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("private-registries/list-org-private-registries", get(list)),
        (
            "private-registries/create-org-private-registry",
            post(create),
        ),
        (
            "private-registries/get-org-public-key",
            get(dependabot::public_key),
        ),
        ("private-registries/get-org-private-registry", get(registry)),
        (
            "private-registries/update-org-private-registry",
            patch(update),
        ),
        (
            "private-registries/delete-org-private-registry",
            delete(remove),
        ),
    ]
}

fn error(error: RegistryError) -> Error {
    match error {
        RegistryError::Missing => Error::missing(),
        RegistryError::Invalid(field) => Error::validation("PrivateRegistry", field, "invalid"),
        RegistryError::Secret(error) => action_secrets::error(error),
        _ => Error::unavailable(error),
    }
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (org, actor) = dependabot::organization(&state, &headers, &org, false).await?;
    let (offset, limit) = page.window()?;
    let (total, configurations) = db::private_registries::list(
        &state.pool,
        actor.user.id,
        org,
        None,
        offset as i64,
        limit as i64,
    )
    .await
    .map_err(error)?;
    let configurations = configurations
        .into_iter()
        .map(|r| r.document())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(error)?;
    Ok((
        page.headers(&uri, (offset.saturating_add(limit) as i64) < total)?,
        Json(json!({"total_count":total, "configurations":configurations})),
    )
        .into_response())
}

async fn registry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (org, actor) = dependabot::organization(&state, &headers, &org, false).await?;
    let registry = db::private_registries::list(&state.pool, actor.user.id, org, Some(&name), 0, 1)
        .await
        .map_err(error)?
        .1
        .into_iter()
        .next()
        .ok_or_else(Error::missing)?;
    Ok(Json(registry.document().map_err(error)?))
}

async fn input(
    state: &AppState,
    actor: i64,
    org: i64,
    mut body: Value,
) -> Result<(Value, Option<String>)> {
    let input = body
        .as_object_mut()
        .ok_or_else(|| Error::invalid("Invalid registry input"))?;
    let value = input.remove("encrypted_value");
    let key = input.remove("key_id");
    let value = match (value, key) {
        (None, None) => None,
        (Some(Value::String(value)), Some(Value::String(id))) => {
            let encryptor = state
                .secret_encryptor
                .as_deref()
                .ok_or_else(|| Error::unavailable(()))?;
            let (key_id, key) =
                db::dependabot::organization_key(&state.pool, encryptor, actor, org)
                    .await
                    .map_err(action_secrets::error)?;
            Some(action_secrets::open_value(&key, &key_id, &id, &value)?)
        }
        _ => {
            return Err(Error::validation(
                "PrivateRegistry",
                "encrypted_value",
                "invalid",
            ))
        }
    };
    Ok((body, value))
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
    body: std::result::Result<Json<Value>, JsonRejection>,
) -> Result<Response> {
    let (org, actor) = dependabot::organization(&state, &headers, &org, true).await?;
    let Json(body) = body.map_err(super::json_error)?;
    let (body, value) = input(&state, actor.user.id, org, body).await?;
    let registry = db::private_registries::save(
        &state.pool,
        state.secret_encryptor.as_deref(),
        actor.user.id,
        &db::users::accounts::AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        org,
        None,
        &body,
        value.as_deref(),
    )
    .await
    .map_err(error)?;
    Ok((
        StatusCode::CREATED,
        Json(registry.document().map_err(error)?),
    )
        .into_response())
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org, name)): Path<(String, String)>,
    body: std::result::Result<Json<Value>, JsonRejection>,
) -> Result<StatusCode> {
    let (org, actor) = dependabot::organization(&state, &headers, &org, true).await?;
    let Json(body) = body.map_err(super::json_error)?;
    let (body, value) = input(&state, actor.user.id, org, body).await?;
    db::private_registries::save(
        &state.pool,
        state.secret_encryptor.as_deref(),
        actor.user.id,
        &db::users::accounts::AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        org,
        Some(&name.to_ascii_uppercase()),
        &body,
        value.as_deref(),
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    let (org, actor) = dependabot::organization(&state, &headers, &org, true).await?;
    db::private_registries::delete(
        &state.pool,
        actor.user.id,
        &db::users::accounts::AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        org,
        &name.to_ascii_uppercase(),
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
