//! Session-bound property controls. The larger body budget is isolated from other organization forms.

use crate::backend::api::http::context::PageContext;
use crate::backend::api::organizations::organization::Fields;
use crate::backend::app::AppState;

use crate::backend::db::organizations::OrganizationOutcome;
use anyhow::{ensure, Result};
use axum::{
    extract::{DefaultBodyLimit, Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use codeyard_webui::pages::orgs as view;
use std::{collections::HashSet, sync::Arc};
use OrganizationOutcome::{Applied, Conflict, Denied, Missing, Suspended};

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/settings/organizations/:id/properties", post(mutate))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
}
pub(super) async fn panel(
    ctx: &PageContext,
    actor: i64,
    org: i64,
) -> std::result::Result<view::PropertyPanel, Response> {
    let definitions =
        match crate::backend::db::organizations::properties::summaries(&ctx.state.pool, actor, org)
            .await
        {
            Ok(Applied(definitions)) => definitions,
            Ok(_) => {
                return Err(crate::backend::api::organizations::organization::not_found(
                    ctx,
                ))
            }
            Err(_) => {
                return Err(crate::backend::api::organizations::organization::unavailable(ctx))
            }
        };
    let selected = if let Some(name) = ctx.query.get("property").filter(|name| !name.is_empty()) {
        let missing = || ctx.error(StatusCode::NOT_FOUND, "Repository property not found.");
        if !crate::backend::db::organizations::properties::valid_name(name) {
            return Err(missing());
        }
        let definition = match crate::backend::db::organizations::properties::definition(
            &ctx.state.pool,
            actor,
            org,
            name,
        )
        .await
        {
            Ok(Applied(Some(definition))) => definition,
            Ok(Applied(None)) => return Err(missing()),
            Ok(_) => {
                return Err(crate::backend::api::organizations::organization::not_found(
                    ctx,
                ))
            }
            Err(_) => {
                return Err(crate::backend::api::organizations::organization::unavailable(ctx))
            }
        };
        let input = &definition.fields;
        Some(view::PropertyDefinition {
            id: definition.id,
            name: input.property_name.clone(),
            value_type: input.value_type.as_str().into(),
            description: input.description.clone().unwrap_or_default(),
            required: input.required,
            require_explicit_values: input.require_explicit_values,
            has_default: input.default_value.is_some(),
            default_value: input
                .default_value
                .as_ref()
                .map(crate::backend::db::organizations::properties::PropertyValue::form_value)
                .unwrap_or_default(),
            allowed_values: input
                .allowed_values
                .as_deref()
                .unwrap_or_default()
                .join("\n"),
            values_editable_by: input.values_editable_by.as_str().into(),
        })
    } else {
        None
    };
    Ok(view::PropertyPanel {
        definitions: definitions
            .into_iter()
            .map(|definition| view::PropertySummary {
                name: definition.property_name,
                value_type: definition.value_type,
                description: definition.description.unwrap_or_default(),
                required: definition.required || definition.require_explicit_values,
            })
            .collect(),
        selected,
    })
}
fn allowed(action: &str) -> &'static [&'static str] {
    match action {
        "save-property" => &[
            "property_name",
            "value_type",
            "description",
            "allowed_values",
            "required",
            "require_explicit_values",
            "use_default",
            "default_value",
            "values_editable_by",
        ],
        "delete-property" => &["property_name", "definition_id", "confirm"],
        "set-property-value" => &["property_name", "repository_names", "value", "unset"],
        _ => &[],
    }
}
fn bound(key: &str) -> usize {
    match key {
        "allowed_values" | "default_value" | "value" => {
            crate::backend::db::organizations::properties::MAX_ALLOWED_VALUES
                * (crate::backend::db::organizations::properties::MAX_VALUE_BYTES + 1)
        }
        "description" => 4096,
        "repository_names" => 8192,
        "property_name" | "confirm" => 1024,
        "csrf_token" => 64,
        _ => 32,
    }
}
fn valid_form(form: &Fields) -> bool {
    let keys = allowed(crate::backend::api::organizations::organization::field(
        form, "action",
    ));
    !keys.is_empty()
        && form.len() <= keys.len() + 2
        && form.iter().all(|(key, value)| {
            (matches!(key.as_str(), "action" | "csrf_token") || keys.contains(&key.as_str()))
                && value.len() <= bound(key)
        })
}
fn flag(form: &Fields, key: &str) -> Result<bool> {
    match form.get(key).map(String::as_str) {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        _ => Err(crate::backend::db::organizations::properties::invalid(
            key_name(key),
            "Invalid property checkbox value",
        )
        .into()),
    }
}
fn key_name(key: &str) -> &'static str {
    match key {
        "required" => "required",
        "require_explicit_values" => "require_explicit_values",
        "use_default" => "default_value",
        _ => "value",
    }
}
/// Native multi-choice controls use one exact value per line, rather than exposing JSON.
async fn apply(
    ctx: &PageContext,
    actor: i64,
    org: i64,
    form: &Fields,
) -> Result<OrganizationOutcome<()>> {
    let name = crate::backend::api::organizations::organization::field(form, "property_name");
    ensure!(
        crate::backend::db::organizations::properties::valid_name(name),
        crate::backend::db::organizations::properties::invalid(
            "property_name",
            "Property names must contain 1-75 ASCII letters, numbers, hyphens or underscores"
        )
    );
    match crate::backend::api::organizations::organization::field(form, "action") {
        "save-property" => {
            let kind = crate::backend::db::organizations::properties::ValueType::parse(
                crate::backend::api::organizations::organization::field(form, "value_type"),
            )?;
            let choices =
                crate::backend::api::organizations::organization::field(form, "allowed_values");
            let input = crate::backend::db::organizations::properties::DefinitionInput {
                property_name: name.into(),
                value_type: kind,
                description: crate::backend::api::organizations::organization::optional(
                    form,
                    "description",
                )
                .map(str::to_owned),
                required: flag(form, "required")?,
                require_explicit_values: flag(form, "require_explicit_values")?,
                default_value: if flag(form, "use_default")? {
                    Some(
                        crate::backend::db::organizations::properties::PropertyValue::from_form(
                            kind,
                            crate::backend::api::organizations::organization::field(
                                form,
                                "default_value",
                            ),
                        ),
                    )
                } else {
                    None
                },
                allowed_values: if kind.selects() || !choices.is_empty() {
                    Some(
                        choices
                            .lines()
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned)
                            .collect(),
                    )
                } else {
                    None
                },
                values_editable_by:
                    crate::backend::db::organizations::properties::EditableBy::parse(
                        crate::backend::api::organizations::organization::optional(
                            form,
                            "values_editable_by",
                        )
                        .unwrap_or("org_actors"),
                    )?,
            };
            Ok(crate::backend::db::organizations::properties::define(
                &ctx.state.pool,
                ctx.database_actor(actor),
                org,
                &[input],
            )
            .await?
            .map(|_| ()))
        }
        "delete-property" => {
            ensure!(crate::backend::api::organizations::organization::field(form,"confirm")==name,crate::backend::db::organizations::properties::invalid("confirm","Type the property name to confirm deleting its definition and every repository value"));
            let definition =
                crate::backend::api::organizations::organization::id(form, "definition_id")
                    .map_err(|()| {
                        crate::backend::db::organizations::properties::invalid(
                            "definition_id",
                            "Reload the property and try again",
                        )
                    })?;
            crate::backend::db::organizations::properties::remove(
                &ctx.state.pool,
                ctx.database_actor(actor),
                org,
                name,
                Some(definition),
            )
            .await
        }
        "set-property-value" => {
            let definition = match crate::backend::db::organizations::properties::definition(
                &ctx.state.pool,
                actor,
                org,
                name,
            )
            .await?
            {
                Applied(Some(definition)) => definition,
                Applied(None) => {
                    return Err(
                        crate::backend::db::organizations::properties::Refusal::Missing.into(),
                    )
                }
                other => return Ok(other.map(|_| ())),
            };
            let repositories =
                crate::backend::api::organizations::organization::field(form, "repository_names")
                    .lines()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
            let value = if flag(form, "unset")? {
                None
            } else {
                Some(
                    crate::backend::db::organizations::properties::PropertyValue::from_form(
                        definition.fields.value_type,
                        crate::backend::api::organizations::organization::field(form, "value"),
                    ),
                )
            };
            crate::backend::db::organizations::properties::set_values(
                &ctx.state.pool,
                ctx.database_actor(actor),
                org,
                &repositories,
                &[crate::backend::db::organizations::properties::ValueInput {
                    property_name: name.into(),
                    value,
                }],
            )
            .await
        }
        _ => Err(crate::backend::db::organizations::properties::invalid(
            "action",
            "Invalid repository property operation",
        )
        .into()),
    }
}
async fn mutate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(org): Path<i64>,
    input: std::result::Result<
        Form<Vec<(String, String)>>,
        axum::extract::rejection::FormRejection,
    >,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    ctx.query.insert("tab".into(), "properties".into());
    let Form(pairs) = match input {
        Ok(form) => form,
        Err(error) => return error.into_response(),
    };
    let mut seen = HashSet::new();
    if pairs.len() > 16 || pairs.iter().any(|(key, _)| !seen.insert(key)) {
        return crate::backend::api::organizations::organization::invalid(&ctx);
    }
    let form: Fields = pairs.into_iter().collect();
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    if !valid_form(&form) {
        return crate::backend::api::organizations::organization::invalid(&ctx);
    }
    if let Err(response) =
        crate::backend::api::organizations::organization::selected(&ctx, actor, org).await
    {
        return response;
    }
    let draft = form
        .iter()
        .filter(|(key, _)| key.as_str() != "csrf_token")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let (status, message) = match apply(&ctx, actor, org, &form).await {
        Ok(Applied(())) => {
            let property =
                if crate::backend::api::organizations::organization::field(&form, "action")
                    == "delete-property"
                {
                    String::new()
                } else {
                    format!(
                        "&property={}",
                        codeyard_webui::encode_component(
                            crate::backend::api::organizations::organization::field(
                                &form,
                                "property_name"
                            )
                        )
                    )
                };
            return ctx.redirect(&format!(
                "{}/{org}?tab=properties{property}",
                crate::backend::api::organizations::INDEX
            ));
        }
        Ok(Denied) => (
            StatusCode::FORBIDDEN,
            "Only organization owners can manage repository properties.".to_owned(),
        ),
        Ok(Missing) => (
            StatusCode::NOT_FOUND,
            "Repository property not found.".to_owned(),
        ),
        Ok(Suspended) => (
            StatusCode::CONFLICT,
            "Organization governance is suspended.".to_owned(),
        ),
        Ok(Conflict) => (
            StatusCode::CONFLICT,
            "Repository properties changed. Reload and try again.".to_owned(),
        ),
        Err(error) => {
            match error.downcast_ref::<crate::backend::db::organizations::properties::Refusal>() {
                Some(crate::backend::db::organizations::properties::Refusal::Invalid {
                    message,
                    ..
                }) => (StatusCode::BAD_REQUEST, (*message).to_owned()),
                Some(crate::backend::db::organizations::properties::Refusal::Denied) => (
                    StatusCode::FORBIDDEN,
                    "Repository property access denied.".to_owned(),
                ),
                Some(crate::backend::db::organizations::properties::Refusal::Missing) => (
                    StatusCode::NOT_FOUND,
                    "Repository property not found.".to_owned(),
                ),
                _ => return crate::backend::api::organizations::organization::unavailable(&ctx),
            }
        }
    };
    ctx.query.remove("property");
    let name = crate::backend::api::organizations::organization::field(&form, "property_name");
    if crate::backend::db::organizations::properties::valid_name(name) {
        if let Ok(Applied(Some(definition))) =
            crate::backend::db::organizations::properties::definition(
                &ctx.state.pool,
                actor,
                org,
                name,
            )
            .await
        {
            ctx.query
                .insert("property".into(), definition.fields.property_name);
        }
    }
    crate::backend::api::organizations::organization::respond(
        &ctx,
        Some(org),
        status,
        Some(message),
        draft,
    )
    .await
}
