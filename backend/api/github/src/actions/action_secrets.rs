//! Sealed-box input is opened once, then stored with the site's encryption key.

use axum::extract::rejection::JsonRejection;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Extension;
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_actions::secrets::SecretKind;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let actions = vec![
        ("actions/list-repo-secrets", get(list)),
        ("actions/get-repo-public-key", get(public_key)),
        ("actions/get-repo-secret", get(secret)),
        ("actions/create-or-update-repo-secret", put(save)),
        ("actions/delete-repo-secret", delete(remove)),
    ];
    let dependabot = vec![
        ("dependabot/list-repo-secrets", get(list)),
        ("dependabot/get-repo-public-key", get(public_key)),
        ("dependabot/get-repo-secret", get(secret)),
        ("dependabot/create-or-update-repo-secret", put(save)),
        ("dependabot/delete-repo-secret", delete(remove)),
    ];
    actions
        .into_iter()
        .map(|(id, route)| (id, route.layer(Extension(SecretKind::Actions))))
        .chain(
            dependabot
                .into_iter()
                .map(|(id, route)| (id, route.layer(Extension(SecretKind::Dependabot)))),
        )
        .collect()
}

pub(super) fn error(error: codeyard_db_core::queries::secrets::SecretsError) -> Error {
    use codeyard_db_core::queries::secrets::SecretsError;
    match error {
        SecretsError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token")
        }
        SecretsError::NotFound => Error::missing(),
        SecretsError::NotSelected => {
            Error::new(StatusCode::CONFLICT, "Secret visibility must be selected")
        }
        SecretsError::InvalidRepositorySelection => {
            Error::validation("Secret", "selected_repository_ids", "invalid")
        }
        SecretsError::MissingValue => {
            Error::validation("Secret", "encrypted_value", "missing_field")
        }
        SecretsError::InvalidName | SecretsError::ReservedName => {
            Error::validation("Secret", "name", "invalid")
        }
        SecretsError::ValueContainsNul
        | SecretsError::ValueTooLarge
        | SecretsError::TooManySecrets
        | SecretsError::TotalTooLarge => Error::invalid(error.to_string()),
        _ => Error::unavailable(error),
    }
}

async fn repository(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    write: bool,
) -> Result<(i64, codeyard_app::api_shared::github::auth::Authentication)> {
    let (repo, auth) =
        codeyard_app::api_shared::github::repository(state, headers, owner, name, write).await?;
    Ok((
        repo.id,
        auth.ok_or_else(codeyard_app::api_shared::github::auth::required)?,
    ))
}

async fn list(
    State(state): State<Arc<AppState>>,
    Extension(kind): Extension<SecretKind>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (repo, actor) = repository(&state, &headers, &owner, &repo, false).await?;
    let secrets =
        codeyard_db_actions::secrets::list_secrets_for(&state.pool, kind, actor.user.id, repo)
            .await
            .map_err(error)?;
    let (offset, limit) = page.window()?;
    let total = secrets.len();
    let secrets: Vec<_> = secrets.into_iter().skip(offset).take(limit).collect();
    Ok((
        page.headers(&uri, offset.saturating_add(limit) < total)?,
        Json(json!({
            "total_count": total, "secrets": secrets,
        })),
    )
        .into_response())
}

pub(super) fn public_key_document(key_id: &str, key: &crypto_box::SecretKey) -> Value {
    json!({"key_id": key_id, "key": STANDARD.encode(key.public_key().as_bytes())})
}

async fn public_key(
    State(state): State<Arc<AppState>>,
    Extension(kind): Extension<SecretKind>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &repo, false).await?;
    let encryptor = state
        .secret_encryptor
        .as_deref()
        .ok_or_else(|| Error::unavailable(()))?;
    let (key_id, key) = codeyard_db_actions::secrets::repository_secret_key_for(
        &state.pool,
        kind,
        encryptor,
        actor.user.id,
        repo,
        &actor.credential(),
    )
    .await
    .map_err(error)?;
    Ok(Json(public_key_document(&key_id, &key)))
}

async fn secret(
    State(state): State<Arc<AppState>>,
    Extension(kind): Extension<SecretKind>,
    headers: HeaderMap,
    Path((owner, repo, name)): Path<(String, String, String)>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &repo, false).await?;
    let name = name.to_ascii_uppercase();
    let secret =
        codeyard_db_actions::secrets::list_secrets_for(&state.pool, kind, actor.user.id, repo)
            .await
            .map_err(error)?
            .into_iter()
            .find(|secret| secret.name == name)
            .ok_or_else(Error::missing)?;
    Ok(Json(json!(secret)))
}

#[derive(Deserialize)]
struct Save {
    key_id: String,
    encrypted_value: String,
}

async fn save(
    State(state): State<Arc<AppState>>,
    Extension(kind): Extension<SecretKind>,
    headers: HeaderMap,
    Path((owner, repo, name)): Path<(String, String, String)>,
    body: std::result::Result<Json<Save>, JsonRejection>,
) -> Result<Response> {
    let (repo, actor) = repository(&state, &headers, &owner, &repo, true).await?;
    let Json(body) = body.map_err(|_| Error::invalid("Invalid secret input"))?;
    let name = name.to_ascii_uppercase();
    codeyard_db_actions::secrets::validate_repository_secret(&name, "").map_err(error)?;
    let encryptor = state
        .secret_encryptor
        .as_deref()
        .ok_or_else(|| Error::unavailable(()))?;
    let (key_id, key) = codeyard_db_actions::secrets::repository_secret_key_for(
        &state.pool,
        kind,
        encryptor,
        actor.user.id,
        repo,
        &actor.credential(),
    )
    .await
    .map_err(error)?;
    let value = open_value(&key, &key_id, &body.key_id, &body.encrypted_value)?;
    let created = codeyard_db_actions::secrets::create_or_update_secret_for(
        &state.pool,
        encryptor,
        (repo, kind),
        &name,
        &value,
        Some(actor.user.id),
        &actor.credential(),
    )
    .await
    .map_err(error)?;
    Ok(if created {
        (StatusCode::CREATED, Json(json!({}))).into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    })
}

pub(super) fn open_value(
    key: &crypto_box::SecretKey,
    expected_key_id: &str,
    key_id: &str,
    encrypted_value: &str,
) -> Result<String> {
    if key_id != expected_key_id {
        return Err(Error::validation("Secret", "key_id", "invalid"));
    }
    // Bound the decoded box before allocating or attempting decryption.
    if encrypted_value.len() > (codeyard_actions::values::MAX_VALUE_BYTES + 48).div_ceil(3) * 4 {
        return Err(Error::validation("Secret", "encrypted_value", "invalid"));
    }
    let invalid = || Error::validation("Secret", "encrypted_value", "invalid");
    let encrypted = STANDARD.decode(encrypted_value).map_err(|_| invalid())?;
    let plaintext = key.unseal(&encrypted).map_err(|_| invalid())?;
    String::from_utf8(plaintext).map_err(|_| invalid())
}

async fn remove(
    State(state): State<Arc<AppState>>,
    Extension(kind): Extension<SecretKind>,
    headers: HeaderMap,
    Path((owner, repo, name)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    let (repo, actor) = repository(&state, &headers, &owner, &repo, true).await?;
    if !codeyard_db_actions::secrets::delete_secret_for(
        &state.pool,
        kind,
        actor.user.id,
        repo,
        &name.to_ascii_uppercase(),
        &actor.credential(),
    )
    .await
    .map_err(error)?
    {
        return Err(Error::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}
