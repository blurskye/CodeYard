use crate::backend::api::http::context::PageContext;
use crate::backend::api::organizations::organization::Fields;
use crate::backend::api::organizations::INDEX;
use crate::backend::app::AppState;

use crate::backend::db::private_registries::Configuration;
use crate::backend::db::private_registries::RegistryError;
use crate::backend::db::secrets::SecretsError;
use axum::{
    extract::{rejection::FormRejection, Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use codeyard_webui::pages::orgs::private_registries as view;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

/// Decoded fields are bounded below; URL encoding may triple them, so this route needs more
/// room than the shared organization form limit.
pub(super) const BODY_LIMIT: usize = 512 * 1024;

pub(super) fn project(
    registry: crate::backend::db::private_registries::Registry,
) -> Result<view::Registry, RegistryError> {
    let config: Configuration = serde_json::from_str(&registry.configuration)?;
    let value = serde_json::to_value(&config)?;
    let mut fields = BTreeMap::new();
    for (name, _) in view::OIDC_FIELDS {
        if name == "selected_repository_ids" {
            fields.insert(
                name.into(),
                config
                    .selected_repository_ids
                    .as_ref()
                    .map(|ids| {
                        ids.iter()
                            .map(i64::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default(),
            );
        } else if let Some(value) = value[name].as_str() {
            fields.insert(name.into(), value.to_owned());
        }
    }
    Ok(view::Registry {
        name: registry.name,
        registry_type: config.registry_type,
        url: config.url,
        visibility: config.visibility,
        auth_type: config.auth_type,
        username: config.username.unwrap_or_default(),
        replaces_base: config.replaces_base,
        fields,
    })
}

fn input(form: &Fields) -> Result<Value, RegistryError> {
    let mut body = json!({});
    for name in ["registry_type", "url", "visibility", "auth_type"] {
        body[name] = json!(crate::backend::api::organizations::organization::field(
            form, name
        ));
    }
    body["username"] = form
        .get("username")
        .filter(|v| !v.is_empty())
        .map_or(Value::Null, |v| json!(v));
    body["replaces_base"] = json!(crate::backend::api::organizations::organization::field(
        form,
        "replaces_base"
    )
    .parse::<bool>()
    .map_err(|_| RegistryError::Invalid("replaces_base"))?);
    for (name, _) in view::OIDC_FIELDS {
        let value = crate::backend::api::organizations::organization::field(form, name);
        if name == "selected_repository_ids" {
            if crate::backend::api::organizations::organization::field(form, "visibility")
                == "selected"
            {
                let ids: Vec<i64> = value
                    .split(',')
                    .filter(|v| !v.trim().is_empty())
                    .map(|v| {
                        v.trim()
                            .parse::<i64>()
                            .map_err(|_| RegistryError::Invalid("selected_repository_ids"))
                    })
                    .collect::<Result<_, _>>()?;
                body[name] = json!(ids);
            }
        } else {
            body[name] = if value.is_empty() {
                Value::Null
            } else {
                json!(value)
            };
        }
    }
    Ok(body)
}

pub(super) async fn post(
    State(state): State<Arc<AppState>>,
    Path(org): Path<i64>,
    headers: HeaderMap,
    uri: Uri,
    form: Result<Form<Fields>, FormRejection>,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    let form = match form {
        Ok(Form(form)) => form,
        Err(_) => return crate::backend::api::organizations::organization::invalid(&ctx),
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    if let Err(response) =
        crate::backend::api::organizations::organization::selected(&ctx, actor, org).await
    {
        return response;
    }
    ctx.query.insert("tab".into(), "private-registries".into());
    let name =
        crate::backend::api::organizations::organization::field(&form, "name").to_ascii_uppercase();
    if !name.is_empty() {
        ctx.query.insert("registry".into(), name.clone());
    }
    let bounded = form.len() <= 28
        && form.iter().all(|(name, value)| {
            let known = matches!(
                name.as_str(),
                "action"
                    | "csrf_token"
                    | "name"
                    | "registry_type"
                    | "url"
                    | "visibility"
                    | "auth_type"
                    | "username"
                    | "value"
                    | "replaces_base"
            ) || view::OIDC_FIELDS.iter().any(|(key, _)| name == key);
            known
                && value.len()
                    <= if name == "value" {
                        16384
                    } else {
                        view::field_max(name)
                    }
        });
    let session =
        crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let credential = crate::backend::db::users::accounts::AccountCredential::Session(&session);
    let result = if !bounded {
        Err(RegistryError::Invalid("configuration"))
    } else {
        match crate::backend::api::organizations::organization::field(&form, "action") {
            "registry-delete" => {
                crate::backend::db::private_registries::delete(
                    &ctx.state.pool,
                    actor,
                    &credential,
                    org,
                    &name,
                )
                .await
            }
            "registry-save" => match input(&form) {
                Ok(body) => {
                    let value = form
                        .get("value")
                        .filter(|v| !v.is_empty())
                        .map(String::as_str);
                    crate::backend::db::private_registries::save(
                        &ctx.state.pool,
                        ctx.state.secret_encryptor.as_deref(),
                        actor,
                        &credential,
                        org,
                        (!name.is_empty()).then_some(name.as_str()),
                        &body,
                        value,
                    )
                    .await
                    .map(|_| ())
                }
                Err(error) => Err(error),
            },
            _ => Err(RegistryError::Invalid("configuration")),
        }
    };
    let status = match result {
        Ok(()) => return ctx.redirect(&format!("{INDEX}/{org}?tab=private-registries")),
        Err(RegistryError::Missing | RegistryError::Secret(SecretsError::NotFound)) => {
            return crate::backend::api::organizations::organization::not_found(&ctx)
        }
        Err(RegistryError::Secret(SecretsError::Denied)) => {
            return ctx.error(
                StatusCode::FORBIDDEN,
                "This organization operation is not permitted.",
            )
        }
        Err(
            RegistryError::Invalid(_)
            | RegistryError::Secret(SecretsError::InvalidRepositorySelection),
        ) => StatusCode::UNPROCESSABLE_ENTITY,
        Err(_) => return crate::backend::api::organizations::organization::unavailable(&ctx),
    };
    let draft = form
        .into_iter()
        .filter(|(key, value)| {
            !matches!(key.as_str(), "value" | "csrf_token") && value.len() <= view::field_max(key)
        })
        .collect();
    crate::backend::api::organizations::organization::respond(&ctx, Some(org), status, Some("Check the registry URL, authentication fields and repository selection. Re-enter the credential if needed.".into()), draft).await
}
