//! Session-bound property controls. The larger body budget is isolated from other organization forms.
use axum::response::IntoResponse;
use codeyard_api_http::cell_routes::CellRoutes;

use crate::organizations::organization::Fields;
use anyhow::ensure;
use anyhow::Result;
use axum::extract::DefaultBodyLimit;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_webui::pages::orgs as view;
use std::collections::HashSet;
use std::sync::Arc;
use OrganizationOutcome::Applied;
use OrganizationOutcome::Conflict;
use OrganizationOutcome::Denied;
use OrganizationOutcome::Missing;
use OrganizationOutcome::Suspended;

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route("/settings/organizations/:id/properties", post(mutate))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
}
pub(super) async fn panel(
    ctx: &PageContext,
    actor: i64,
    org: i64,
) -> std::result::Result<view::PropertyPanel, Response> {
    let definitions =
        match codeyard_db_orgs::organizations::properties::summaries(&ctx.state.pool, actor, org)
            .await
        {
            Ok(Applied(definitions)) => definitions,
            Ok(_) => return Err(crate::organizations::organization::not_found(ctx)),
            Err(_) => return Err(crate::organizations::organization::unavailable(ctx)),
        };
    let selected = if let Some(name) = ctx.query.get("property").filter(|name| !name.is_empty()) {
        let missing = || ctx.error(StatusCode::NOT_FOUND, "Repository property not found.");
        if !codeyard_db_core::queries::organizations::properties::valid_name(name) {
            return Err(missing());
        }
        let definition = match codeyard_db_orgs::organizations::properties::definition(
            &ctx.state.pool,
            actor,
            org,
            name,
        )
        .await
        {
            Ok(Applied(Some(definition))) => definition,
            Ok(Applied(None)) => return Err(missing()),
            Ok(_) => return Err(crate::organizations::organization::not_found(ctx)),
            Err(_) => return Err(crate::organizations::organization::unavailable(ctx)),
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
                .map(
                    codeyard_db_core::queries::organizations::properties::PropertyValue::form_value,
                )
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
            codeyard_db_core::queries::organizations::properties::MAX_ALLOWED_VALUES
                * (codeyard_db_core::queries::organizations::properties::MAX_VALUE_BYTES + 1)
        }
        "description" => 4096,
        "repository_names" => 8192,
        "property_name" | "confirm" => 1024,
        "csrf_token" => 64,
        _ => 32,
    }
}
fn valid_form(form: &Fields) -> bool {
    let keys = allowed(codeyard_api_http::shared::user::responses::field(
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
        _ => Err(
            codeyard_db_core::queries::organizations::properties::invalid(
                key_name(key),
                "Invalid property checkbox value",
            )
            .into(),
        ),
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
    let name = codeyard_api_http::shared::user::responses::field(form, "property_name");
    ensure!(
        codeyard_db_core::queries::organizations::properties::valid_name(name),
        codeyard_db_core::queries::organizations::properties::invalid(
            "property_name",
            "Property names must contain 1-75 ASCII letters, numbers, hyphens or underscores"
        )
    );
    match codeyard_api_http::shared::user::responses::field(form, "action") {
        "save-property" => {
            let kind = codeyard_db_core::queries::organizations::properties::ValueType::parse(
                codeyard_api_http::shared::user::responses::field(form, "value_type"),
            )?;
            let choices = codeyard_api_http::shared::user::responses::field(form, "allowed_values");
            let input = codeyard_db_core::queries::organizations::properties::DefinitionInput {
                property_name: name.into(),
                value_type: kind,
                description: crate::organizations::organization::optional(form, "description")
                    .map(str::to_owned),
                required: flag(form, "required")?,
                require_explicit_values: flag(form, "require_explicit_values")?,
                default_value: if flag(form, "use_default")? {
                    Some(
                        codeyard_db_core::queries::organizations::properties::PropertyValue::from_form(
                            kind,
                            codeyard_api_http::shared::user::responses::field(
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
                    codeyard_db_core::queries::organizations::properties::EditableBy::parse(
                        crate::organizations::organization::optional(form, "values_editable_by")
                            .unwrap_or("org_actors"),
                    )?,
            };
            Ok(codeyard_db_orgs::organizations::properties::define(
                &ctx.state.pool,
                ctx.database_actor(actor),
                org,
                &[input],
            )
            .await?
            .map(|_| ()))
        }
        "delete-property" => {
            ensure!(codeyard_api_http::shared::user::responses::field(form,"confirm")==name,codeyard_db_core::queries::organizations::properties::invalid("confirm","Type the property name to confirm deleting its definition and every repository value"));
            let definition = crate::organizations::organization::id(form, "definition_id")
                .map_err(|()| {
                    codeyard_db_core::queries::organizations::properties::invalid(
                        "definition_id",
                        "Reload the property and try again",
                    )
                })?;
            codeyard_db_orgs::organizations::properties::remove(
                &ctx.state.pool,
                ctx.database_actor(actor),
                org,
                name,
                Some(definition),
            )
            .await
        }
        "set-property-value" => {
            let definition = match codeyard_db_orgs::organizations::properties::definition(
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
                        codeyard_db_core::queries::organizations::properties::Refusal::Missing
                            .into(),
                    )
                }
                other => return Ok(other.map(|_| ())),
            };
            let repositories =
                codeyard_api_http::shared::user::responses::field(form, "repository_names")
                    .lines()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
            let value = if flag(form, "unset")? {
                None
            } else {
                Some(
                    codeyard_db_core::queries::organizations::properties::PropertyValue::from_form(
                        definition.fields.value_type,
                        codeyard_api_http::shared::user::responses::field(form, "value"),
                    ),
                )
            };
            codeyard_db_orgs::organizations::properties::set_values(
                &ctx.state.pool,
                ctx.database_actor(actor),
                org,
                &repositories,
                &[
                    codeyard_db_core::queries::organizations::properties::ValueInput {
                        property_name: name.into(),
                        value,
                    },
                ],
            )
            .await
        }
        _ => Err(
            codeyard_db_core::queries::organizations::properties::invalid(
                "action",
                "Invalid repository property operation",
            )
            .into(),
        ),
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
        return crate::organizations::organization::invalid(&ctx);
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
        return crate::organizations::organization::invalid(&ctx);
    }
    if let Err(response) = crate::organizations::organization::selected(&ctx, actor, org).await {
        return response;
    }
    let draft = form
        .iter()
        .filter(|(key, _)| key.as_str() != "csrf_token")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let (status, message) = match apply(&ctx, actor, org, &form).await {
        Ok(Applied(())) => {
            let property = if codeyard_api_http::shared::user::responses::field(&form, "action")
                == "delete-property"
            {
                String::new()
            } else {
                format!(
                    "&property={}",
                    codeyard_webui::encode_component(
                        codeyard_api_http::shared::user::responses::field(&form, "property_name")
                    )
                )
            };
            return ctx.redirect(&format!(
                "{}/{org}?tab=properties{property}",
                crate::organizations::INDEX
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
            match error
                .downcast_ref::<codeyard_db_core::queries::organizations::properties::Refusal>()
            {
                Some(codeyard_db_core::queries::organizations::properties::Refusal::Invalid {
                    message,
                    ..
                }) => (StatusCode::BAD_REQUEST, (*message).to_owned()),
                Some(codeyard_db_core::queries::organizations::properties::Refusal::Denied) => (
                    StatusCode::FORBIDDEN,
                    "Repository property access denied.".to_owned(),
                ),
                Some(codeyard_db_core::queries::organizations::properties::Refusal::Missing) => (
                    StatusCode::NOT_FOUND,
                    "Repository property not found.".to_owned(),
                ),
                _ => return crate::organizations::organization::unavailable(&ctx),
            }
        }
    };
    ctx.query.remove("property");
    let name = codeyard_api_http::shared::user::responses::field(&form, "property_name");
    if codeyard_db_core::queries::organizations::properties::valid_name(name) {
        if let Ok(Applied(Some(definition))) =
            codeyard_db_orgs::organizations::properties::definition(
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
    crate::organizations::organization::respond(&ctx, Some(org), status, Some(message), draft).await
}
