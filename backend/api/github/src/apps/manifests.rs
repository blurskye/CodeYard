//! One-time, session-bound manifest conversion. Only public verification keys persist.
use crate::apps::installations::app_value;
use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_db_identity::github_apps::App;
use codeyard_security::token_hash::token_hash;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use codeyard_webhooks::settings::HookError;
use codeyard_webhooks::settings::HookForm;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

/// GitHub documents this conversion's 422 as `validation-error-simple`.
pub(super) async fn convert(
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
) -> Result<(StatusCode, Json<Value>)> {
    if code.len() > 128 {
        return Err(Error::missing());
    }
    let hash = token_hash(&code);
    let draft = codeyard_db_identity::github_app_manifests::draft(&state.pool, &hash, true)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let value: Value = serde_json::from_str(&draft.manifest).map_err(Error::unavailable)?;
    let manifest = codeyard_identity::github_apps::manifest(&value).ok_or_else(|| {
        Error::invalid_simple(
            "Invalid manifest or unsupported permissions, events or setup automation",
        )
    })?;
    let webhook_secret = manifest
        .hook_url
        .as_ref()
        .map(|_| codeyard_security::crypto::random_hex());
    let fields = if let Some(url) = &manifest.hook_url {
        let form = HookForm {
            username: None,
            password: None,
            clear_password: false,
            clear_secret: false,
            payload_url: url.clone(),
            content_type: codeyard_webhooks::settings::JSON_CONTENT_TYPE.into(),
            events: vec!["*".into()],
            active: manifest.hook_active,
            ssl_verify: true,
            secret: webhook_secret.clone().unwrap_or_default(),
        };
        let mut fields =
            codeyard_webhooks::settings::validated(&form).map_err(|error| match error {
                HookError::Invalid(message) => Error::invalid_simple(message),
                _ => Error::unavailable(()),
            })?;
        fields.events = manifest.events.clone();
        Some(fields)
    } else {
        None
    };
    let sealed = codeyard_webhooks::settings::seal(
        state.secret_encryptor.as_deref(),
        webhook_secret.as_deref().unwrap_or(""),
    )
    .map_err(|_| Error::unavailable(()))?;
    // The response is built without database reads after conversion, which deletes the code:
    // a failed read then would lose the only copy of the new credentials.
    let owner = codeyard_db_identity::users::accounts::get_user_by_id(
        &state.pool,
        draft.user_id.ok_or_else(Error::missing)?,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(Error::missing)?;
    let (private, public) = codeyard_identity::github_apps::generate_key()
        .await
        .map_err(Error::unavailable)?;
    let client_secret = codeyard_security::crypto::random_hex();
    let app = App {
        id: 0,
        owner_id: draft.user_id.ok_or_else(Error::missing)?,
        bot_id: 0,
        is_public: manifest.is_public,
        client_id: format!("Iv1.{}", &codeyard_security::crypto::random_hex()[..16]),
        slug: manifest.slug,
        name: manifest.name,
        description: manifest.description,
        homepage: manifest.homepage,
        public_key: public,
        permissions: manifest.permissions.to_string(),
        events: manifest.events,
        access_level: if codeyard_types::github_apps::allows(
            &manifest.permissions,
            "administration",
            true,
        ) {
            5
        } else if codeyard_identity::github_apps::write(&manifest.permissions) {
            3
        } else {
            1
        },
        created_at: chrono::Utc::now(),
    };
    let app = codeyard_db_identity::github_app_manifests::convert(
        &state.pool,
        &hash,
        &app,
        &token_hash(&client_secret),
        &manifest.callbacks,
        fields.as_ref(),
        sealed.as_ref(),
    )
    .await
    .map_err(|error| {
        let unique = error
            .downcast_ref::<sqlx::Error>()
            .and_then(sqlx::Error::as_database_error)
            .is_some_and(|error| error.is_unique_violation());
        if unique {
            Error::invalid_simple("App name is already registered")
        } else {
            tracing::warn!(%error,"App manifest conversion failed");
            Error::unavailable(error)
        }
    })?
    .ok_or_else(Error::missing)?;
    let mut value = app_value(&app, &owner, 0)?;
    value["client_secret"] = json!(client_secret);
    value["webhook_secret"] = json!(webhook_secret);
    value["pem"] = json!(private);
    Ok((StatusCode::CREATED, Json(value)))
}
