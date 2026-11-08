use crate::access::check_repo_admin_access;
use crate::access::find_repository;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_current_user;
use codeyard_app::app::AppState;
use serde::Deserialize;
use serde::Serialize;
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

pub(crate) fn secret_error(
    error: codeyard_database::queries::secrets::SecretsError,
) -> (StatusCode, Json<ApiError>) {
    match error {
        codeyard_database::queries::secrets::SecretsError::Denied => (
            StatusCode::FORBIDDEN,
            Json(ApiError::new(
                "forbidden",
                "Repository secret access denied",
            )),
        ),
        codeyard_database::queries::secrets::SecretsError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Secret not found")),
        ),
        codeyard_database::queries::secrets::SecretsError::InvalidName => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Secret names must be 1 to 255 ASCII letters, digits, or underscores and start with a letter or underscore",
            )),
        ),
        codeyard_database::queries::secrets::SecretsError::ReservedName => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Secret names beginning with GITHUB_ are reserved",
            )),
        ),
        codeyard_database::queries::secrets::SecretsError::ValueContainsNul => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Secret values cannot contain NUL",
            )),
        ),
        codeyard_database::queries::secrets::SecretsError::ValueTooLarge => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Secret values cannot exceed 48 KiB",
            )),
        ),
        codeyard_database::queries::secrets::SecretsError::TooManySecrets => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "A repository can have at most 100 secrets",
            )),
        ),
        codeyard_database::queries::secrets::SecretsError::TotalTooLarge => (
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

    codeyard_db_actions::secrets::create_or_update_secret(
        &state.pool,
        encryptor,
        repository.id,
        &name,
        &body.value,
        Some(user.id),
        &codeyard_database::queries::users::accounts::AccountCredential::Session(
            &codeyard_api_http::session::extract_session_id(&headers).unwrap_or_default(),
        ),
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
    let secrets = codeyard_db_actions::secrets::list_secrets(&state.pool, user.id, repository.id)
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
    let deleted = codeyard_db_actions::secrets::delete_secret(
        &state.pool,
        user.id,
        repository.id,
        &name,
        &codeyard_database::queries::users::accounts::AccountCredential::Session(
            &codeyard_api_http::session::extract_session_id(&headers).unwrap_or_default(),
        ),
    )
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

pub(crate) async fn form(
    ctx: &codeyard_api_http::context::PageContext,
    loaded: &codeyard_api_http::shared::repos::chrome::Loaded,
    action: &str,
    form: &std::collections::HashMap<String, String>,
) -> axum::response::Response {
    use codeyard_api_http::shared::repos::forms::field;
    let Some(actor) = ctx.user.as_ref() else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to manage secrets.");
    };
    let (kind, tab) = match field(form, "secret_kind").as_str() {
        "" | "actions" => (codeyard_db_actions::secrets::SecretKind::Actions, "secrets"),
        "dependabot" => (
            codeyard_db_actions::secrets::SecretKind::Dependabot,
            "dependabot-secrets",
        ),
        _ => return ctx.error(StatusCode::BAD_REQUEST, "Invalid secret kind."),
    };
    // Saving stores the name uppercase. Deleting uses the exact stored name the form lists, so
    // an older lowercase secret never deletes its uppercase neighbour.
    let name = field(form, "name");
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let credential =
        codeyard_database::queries::users::accounts::AccountCredential::Session(&session);
    let result = if action == "secret-save" {
        match ctx.state.secret_encryptor.as_deref() {
            Some(encryptor) => codeyard_db_actions::secrets::create_or_update_secret_for(
                &ctx.state.pool,
                encryptor,
                (loaded.repository.id, kind),
                &name,
                &field(form, "value"),
                Some(actor.id),
                &credential,
            )
            .await
            .map(|_| ())
            .map_err(secret_error),
            None => Err(secrets_unavailable()),
        }
    } else {
        codeyard_db_actions::secrets::delete_secret_for(
            &ctx.state.pool,
            kind,
            actor.id,
            loaded.repository.id,
            &name,
            &credential,
        )
        .await
        .map_err(secret_error)
        .and_then(|deleted| {
            if deleted {
                Ok(())
            } else {
                Err(secret_error(
                    codeyard_database::queries::secrets::SecretsError::NotFound,
                ))
            }
        })
    };
    match result {
        Ok(()) => ctx.redirect(&format!("{}/settings?tab={tab}", loaded.chrome.base())),
        Err((status, Json(error))) => ctx.repo_error(&loaded.chrome, status, &error.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_errors_are_typed_and_sanitized() {
        let (status, Json(error)) =
            secret_error(codeyard_database::queries::secrets::SecretsError::Denied);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(error.error, "forbidden");
        for (error, message) in [
            (
                codeyard_database::queries::secrets::SecretsError::InvalidName,
                "Secret names must be 1 to 255 ASCII letters, digits, or underscores and start with a letter or underscore",
            ),
            (
                codeyard_database::queries::secrets::SecretsError::ReservedName,
                "Secret names beginning with GITHUB_ are reserved",
            ),
            (
                codeyard_database::queries::secrets::SecretsError::ValueContainsNul,
                "Secret values cannot contain NUL",
            ),
            (
                codeyard_database::queries::secrets::SecretsError::ValueTooLarge,
                "Secret values cannot exceed 48 KiB",
            ),
            (
                codeyard_database::queries::secrets::SecretsError::TooManySecrets,
                "A repository can have at most 100 secrets",
            ),
            (
                codeyard_database::queries::secrets::SecretsError::TotalTooLarge,
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
            codeyard_database::queries::secrets::SecretsError::Authority(anyhow::anyhow!(
                "private SQL encrypted_value nonce plaintext /filesystem/key"
            )),
            codeyard_database::queries::secrets::SecretsError::Encryption(
                codeyard_security::secrets::SecretError::EncryptionFailed,
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
