use super::{
    organization::{self, Fields},
    INDEX,
};
use crate::{
    backend::{
        api::http::context::PageContext,
        app::AppState,
        db::{
            self,
            private_registries::{Configuration, RegistryError},
            secrets::SecretsError,
        },
    },
    webui::pages::orgs::private_registries as view,
};
use axum::{
    extract::{rejection::FormRejection, Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

pub(super) fn project(
    registry: db::private_registries::Registry,
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
        body[name] = json!(organization::field(form, name));
    }
    body["username"] = form
        .get("username")
        .filter(|v| !v.is_empty())
        .map_or(Value::Null, |v| json!(v));
    body["replaces_base"] = json!(organization::field(form, "replaces_base")
        .parse::<bool>()
        .map_err(|_| RegistryError::Invalid("replaces_base"))?);
    for (name, _) in view::OIDC_FIELDS {
        let value = organization::field(form, name);
        if name == "selected_repository_ids" {
            if organization::field(form, "visibility") == "selected" {
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
            } else if !value.trim().is_empty() {
                return Err(RegistryError::Invalid("selected_repository_ids"));
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
        Err(_) => return organization::invalid(&ctx),
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    if let Err(response) = organization::selected(&ctx, actor, org).await {
        return response;
    }
    ctx.query.insert("tab".into(), "private-registries".into());
    let name = organization::field(&form, "name").to_ascii_uppercase();
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
            known && value.len() <= if name == "value" { 16384 } else { 4096 }
        });
    let session = crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let credential = db::users::accounts::AccountCredential::Session(&session);
    let result = if !bounded {
        Err(RegistryError::Invalid("configuration"))
    } else {
        match organization::field(&form, "action") {
            "registry-delete" => {
                db::private_registries::delete(&ctx.state.pool, actor, &credential, org, &name).await
            }
            "registry-save" => match input(&form) {
                Ok(body) => {
                    let value = form
                        .get("value")
                        .filter(|v| !v.is_empty())
                        .map(String::as_str);
                    db::private_registries::save(
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
            return organization::not_found(&ctx)
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
        Err(_) => return organization::unavailable(&ctx),
    };
    let draft = form
        .into_iter()
        .filter(|(key, value)| {
            !matches!(key.as_str(), "value" | "csrf_token") && value.len() <= 4096
        })
        .collect();
    organization::respond(&ctx, Some(org), status, Some("Check the registry URL, authentication fields and repository selection. Re-enter the credential if needed.".into()), draft).await
}
