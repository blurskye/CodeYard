use axum::extract::rejection::JsonRejection;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_actions::private_registries::RegistryError;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use serde_json::json;
use serde_json::Value;
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
            get(crate::dependabot::public_key),
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
        RegistryError::Secret(error) => crate::actions::action_secrets::error(error),
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
    let (org, actor) = crate::dependabot::organization(&state, &headers, &org, false).await?;
    let (offset, limit) = page.window()?;
    let (total, configurations) = codeyard_db_actions::private_registries::list(
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
    let (org, actor) = crate::dependabot::organization(&state, &headers, &org, false).await?;
    let registry = codeyard_db_actions::private_registries::list(
        &state.pool,
        actor.user.id,
        org,
        Some(&name),
        0,
        1,
    )
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
            let (key_id, key) = codeyard_db_security::dependabot::organization_key(
                &state.pool,
                encryptor,
                actor,
                org,
            )
            .await
            .map_err(crate::actions::action_secrets::error)?;
            Some(crate::actions::action_secrets::open_value(
                &key, &key_id, &id, &value,
            )?)
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
    let (org, actor) = crate::dependabot::organization(&state, &headers, &org, true).await?;
    let Json(body) = body.map_err(codeyard_types::github_error::json_error)?;
    let (body, value) = input(&state, actor.user.id, org, body).await?;
    let registry = codeyard_db_actions::private_registries::save(
        &state.pool,
        state.secret_encryptor.as_deref(),
        actor.user.id,
        &codeyard_database::queries::users::accounts::AccountCredential::Token(
            actor.token.id,
            &actor.token.token_hash,
        ),
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
    let (org, actor) = crate::dependabot::organization(&state, &headers, &org, true).await?;
    let Json(body) = body.map_err(codeyard_types::github_error::json_error)?;
    let (body, value) = input(&state, actor.user.id, org, body).await?;
    codeyard_db_actions::private_registries::save(
        &state.pool,
        state.secret_encryptor.as_deref(),
        actor.user.id,
        &codeyard_database::queries::users::accounts::AccountCredential::Token(
            actor.token.id,
            &actor.token.token_hash,
        ),
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
    let (org, actor) = crate::dependabot::organization(&state, &headers, &org, true).await?;
    codeyard_db_actions::private_registries::delete(
        &state.pool,
        actor.user.id,
        &codeyard_database::queries::users::accounts::AccountCredential::Token(
            actor.token.id,
            &actor.token.token_hash,
        ),
        org,
        &name.to_ascii_uppercase(),
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
