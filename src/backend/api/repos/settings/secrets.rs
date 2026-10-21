use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::{check_repo_admin_access, find_repository};
use crate::backend::app::AppState;
use crate::backend::db;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Deserialize)]
pub struct CreateSecretRequest {
    pub value: String,
}

#[derive(Serialize)]
pub struct SecretResponse {
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

fn secrets_unavailable() -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError::new(
            "secret_unavailable",
            "Repository secrets unavailable",
        )),
    )
}

fn secret_error(error: db::secrets::SecretsError) -> (StatusCode, Json<ApiError>) {
    match error {
        db::secrets::SecretsError::Denied => (
            StatusCode::FORBIDDEN,
            Json(ApiError::new(
                "forbidden",
                "Repository secret access denied",
            )),
        ),
        db::secrets::SecretsError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Secret not found")),
        ),
        db::secrets::SecretsError::InvalidName => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Secret names must be 1 to 255 ASCII letters, digits, or underscores and start with a letter or underscore",
            )),
        ),
        db::secrets::SecretsError::ReservedName => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Secret names beginning with GITHUB_ are reserved",
            )),
        ),
        db::secrets::SecretsError::ValueContainsNul => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Secret values cannot contain NUL",
            )),
        ),
        db::secrets::SecretsError::ValueTooLarge => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Secret values cannot exceed 48 KiB",
            )),
        ),
        db::secrets::SecretsError::TooManySecrets => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "A repository can have at most 100 secrets",
            )),
        ),
        db::secrets::SecretsError::TotalTooLarge => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "A repository's secrets can total at most 1 MiB",
            )),
        ),
        _ => secrets_unavailable(),
    }
}

pub async fn create_or_update_secret(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, name)): Path<(String, String, String)>,
    Json(body): Json<CreateSecretRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &user).await?;
    let encryptor = state
        .secret_encryptor
        .as_ref()
        .ok_or_else(secrets_unavailable)?;

    db::secrets::create_or_update_secret(
        &state.pool,
        encryptor,
        repository.id,
        &name,
        &body.value,
        Some(user.id),
    )
    .await
    .map_err(secret_error)?;

    Ok((StatusCode::NO_CONTENT, ()))
}

pub async fn list_secrets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &user).await?;
    let secrets = db::secrets::list_secrets(&state.pool, user.id, repository.id)
        .await
        .map_err(secret_error)?;

    Ok(Json(
        secrets
            .into_iter()
            .map(|secret| SecretResponse {
                name: secret.name,
                created_at: secret.created_at.to_rfc3339(),
                updated_at: secret.updated_at.to_rfc3339(),
            })
            .collect::<Vec<_>>(),
    ))
}

pub async fn delete_secret(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, name)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &user).await?;
    let deleted = db::secrets::delete_secret(&state.pool, user.id, repository.id, &name)
        .await
        .map_err(secret_error)?;

    if deleted {
        Ok((StatusCode::NO_CONTENT, ()))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Secret not found")),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_errors_are_typed_and_sanitized() {
        let (status, Json(error)) = secret_error(db::secrets::SecretsError::Denied);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(error.error, "forbidden");
        for (error, message) in [
            (
                db::secrets::SecretsError::InvalidName,
                "Secret names must be 1 to 255 ASCII letters, digits, or underscores and start with a letter or underscore",
            ),
            (
                db::secrets::SecretsError::ReservedName,
                "Secret names beginning with GITHUB_ are reserved",
            ),
            (
                db::secrets::SecretsError::ValueContainsNul,
                "Secret values cannot contain NUL",
            ),
            (
                db::secrets::SecretsError::ValueTooLarge,
                "Secret values cannot exceed 48 KiB",
            ),
            (
                db::secrets::SecretsError::TooManySecrets,
                "A repository can have at most 100 secrets",
            ),
            (
                db::secrets::SecretsError::TotalTooLarge,
                "A repository's secrets can total at most 1 MiB",
            ),
        ] {
            let (status, Json(error)) = secret_error(error);
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(error.error, "validation_error");
            assert_eq!(error.message, message);
            let body = serde_json::to_string(&error).unwrap();
            for forbidden in ["private-value", "BAD-NAME", "SQL", "nonce"] {
                assert!(!body.contains(forbidden));
            }
        }
        for error in [
            db::secrets::SecretsError::Authority(anyhow::anyhow!(
                "private SQL encrypted_value nonce plaintext /filesystem/key"
            )),
            db::secrets::SecretsError::Encryption(
                crate::backend::crypto::SecretError::EncryptionFailed,
            ),
        ] {
            let (status, Json(error)) = secret_error(error);
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(error.error, "secret_unavailable");
            let body = serde_json::to_string(&error).unwrap();
            for forbidden in [
                "SQL",
                "encrypted_value",
                "nonce",
                "plaintext",
                "/filesystem",
                "key",
                "SECRETS_MASTER_KEY",
            ] {
                assert!(!body.contains(forbidden));
            }
        }
    }
}
