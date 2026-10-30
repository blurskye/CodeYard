//! One-time, session-bound manifest conversion. Only public verification keys persist.
use super::{
    super::{Error, Result},
    installations::app_value,
};
use crate::backend::{
    app::AppState,
    db::{self, github_apps::App},
    identity::{github_apps as apps, tokens::token_hash},
    webhooks::settings::{self, HookError, HookForm},
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};
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
    let draft = db::github_app_manifests::draft(&state.pool, &hash, true)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let value: Value = serde_json::from_str(&draft.manifest).map_err(Error::unavailable)?;
    let manifest = apps::manifest(&value).ok_or_else(|| {
        Error::invalid_simple("Invalid manifest or unsupported permissions, events or setup automation")
    })?;
    let webhook_secret = manifest.hook_url.as_ref().map(|_| crate::backend::crypto::random_hex());
    let fields = if let Some(url) = &manifest.hook_url {
        let form = HookForm {
            username: None,
            password: None,
            clear_password: false,
            clear_secret: false,
            payload_url: url.clone(),
            content_type: settings::JSON_CONTENT_TYPE.into(),
            events: vec!["*".into()],
            active: manifest.hook_active,
            ssl_verify: true,
            secret: webhook_secret.clone().unwrap_or_default(),
        };
        let mut fields = settings::validated(&form).map_err(|error| match error {
            HookError::Invalid(message) => Error::invalid_simple(message),
            _ => Error::unavailable(()),
        })?;
        fields.events = manifest.events.clone();
        Some(fields)
    } else {
        None
    };
    let sealed = settings::seal(
        state.secret_encryptor.as_deref(),
        webhook_secret.as_deref().unwrap_or(""),
    )
    .map_err(|_| Error::unavailable(()))?;
    // The response is built without database reads after conversion, which deletes the code:
    // a failed read then would lose the only copy of the new credentials.
    let owner =
        db::users::accounts::get_user_by_id(&state.pool, draft.user_id.ok_or_else(Error::missing)?)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    let (private, public) = apps::generate_key().await.map_err(Error::unavailable)?;
    let client_secret = crate::backend::crypto::random_hex();
    let app = App {
        id: 0,
        owner_id: draft.user_id.ok_or_else(Error::missing)?,
        bot_id: 0,
        is_public: manifest.is_public,
        client_id: format!("Iv1.{}", &crate::backend::crypto::random_hex()[..16]),
        slug: manifest.slug,
        name: manifest.name,
        description: manifest.description,
        homepage: manifest.homepage,
        public_key: public,
        permissions: manifest.permissions.to_string(),
        events: manifest.events,
        access_level: if apps::allows(&manifest.permissions, "administration", true) {
            5
        } else if apps::write(&manifest.permissions) {
            3
        } else {
            1
        },
        created_at: chrono::Utc::now(),
    };
    let app = db::github_app_manifests::convert(
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
