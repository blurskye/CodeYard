//! Native metadata forms use the same policy and transactions as REST and creation.
use super::{chrome::Loaded, repository};
use crate::{
    backend::{api::http::context::PageContext, db::organizations::properties as data},
    webui::{
        components::repository_properties::RepositoryPropertyField,
        pages::repos::settings::properties as view,
    },
};
use axum::{http::StatusCode, response::Response};
use std::collections::{HashMap, HashSet};

pub(super) async fn panel(
    ctx: &PageContext,
    loaded: &Loaded,
    name: Option<&str>,
) -> Result<view::Properties, Response> {
    let metadata = data::repository(
        &ctx.state.pool,
        ctx.user.as_ref().map(|user| user.id),
        loaded.repository.id,
    )
    .await
    .map_err(|_| {
        ctx.repo_error(
            &loaded.chrome,
            StatusCode::SERVICE_UNAVAILABLE,
            "Repository properties are unavailable.",
        )
    })?;
    let Some(metadata) = metadata else {
        return Ok(view::Properties::default());
    };
    let selected = if let Some(name) = name.filter(|name| !name.is_empty()) {
        let definition = metadata
            .definitions
            .iter()
            .find(|row| row.fields.property_name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                ctx.repo_error(
                    &loaded.chrome,
                    StatusCode::NOT_FOUND,
                    "Repository property not found.",
                )
            })?;
        let fields = &definition.fields;
        let value = metadata
            .values
            .properties
            .iter()
            .find(|row| row.property_name.eq_ignore_ascii_case(name))
            .and_then(|row| row.value.as_ref())
            .map(data::PropertyValue::form_value);
        Some(view::Property {
            id: definition.id,
            field: RepositoryPropertyField {
                name: fields.property_name.clone(),
                value_type: fields.value_type.as_str().into(),
                description: fields.description.clone().unwrap_or_default(),
                allowed_values: fields.allowed_values.clone().unwrap_or_default(),
                required: fields.needs_explicit_value(),
                editable: metadata.organization_owner
                    || (metadata.repository_admin
                        && fields.values_editable_by == data::EditableBy::OrgAndRepoActors),
                default_preview: None,
            },
            draft: value.clone().unwrap_or_default(),
            value,
            can_unset: !fields.needs_explicit_value(),
        })
    } else {
        None
    };
    Ok(view::Properties {
        names: metadata
            .definitions
            .iter()
            .map(|row| row.fields.property_name.clone())
            .collect(),
        selected,
    })
}
pub(super) async fn form(
    ctx: &PageContext,
    loaded: &Loaded,
    action: &str,
    form: &HashMap<String, String>,
    pairs: &[(String, String)],
) -> Response {
    let mut keys = HashSet::new();
    if pairs.len() > 5
        || pairs.iter().any(|(name, _)| {
            !matches!(
                name.as_str(),
                "csrf_token" | "repository_id" | "definition_id" | "property_name" | "value"
            ) || !keys.insert(name)
        })
    {
        return ctx.repo_error(
            &loaded.chrome,
            StatusCode::BAD_REQUEST,
            "Invalid repository property form.",
        );
    }
    let name = form.get("property_name").map(String::as_str).unwrap_or("");
    let raw = form.get("value").map(String::as_str).unwrap_or("");
    if !data::valid_name(name) || raw.len() > data::MAX_FORM_VALUE_BYTES {
        return ctx.repo_error(
            &loaded.chrome,
            StatusCode::BAD_REQUEST,
            "Invalid repository property input.",
        );
    }
    let Some(id) = form
        .get("definition_id")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|id| *id > 0)
    else {
        return ctx.repo_error(
            &loaded.chrome,
            StatusCode::BAD_REQUEST,
            "Invalid repository property identity.",
        );
    };
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    let metadata = match data::repository(&ctx.state.pool, Some(actor), loaded.repository.id).await
    {
        Ok(Some(metadata)) => metadata,
        Ok(None) => {
            return ctx.repo_error(
                &loaded.chrome,
                StatusCode::NOT_FOUND,
                "Repository properties not found.",
            )
        }
        Err(_) => {
            return ctx.repo_error(
                &loaded.chrome,
                StatusCode::SERVICE_UNAVAILABLE,
                "Repository properties are unavailable.",
            )
        }
    };
    let Some(definition) = metadata
        .definitions
        .iter()
        .find(|row| row.id == id && row.fields.property_name.eq_ignore_ascii_case(name))
    else {
        return ctx.repo_error(
            &loaded.chrome,
            StatusCode::CONFLICT,
            "The repository property changed. Reload before retrying.",
        );
    };
    let value = if action == "repository-property-unset" {
        None
    } else {
        if !form.contains_key("value") {
            return ctx.repo_error(
                &loaded.chrome,
                StatusCode::BAD_REQUEST,
                "Provide a repository property value.",
            );
        }
        Some(data::PropertyValue::from_form(
            definition.fields.value_type,
            raw,
        ))
    };
    let input = data::ValueInput {
        property_name: name.to_owned(),
        value,
    };
    let outcome = data::set_repository_value(
        &ctx.state.pool,
        ctx.database_actor(actor),
        loaded.repository.id,
        id,
        &input,
    )
    .await;
    let (status, message) = match outcome {
        Ok(()) => {
            return ctx.redirect(&format!(
                "{}/settings?tab=properties&property={}",
                loaded.chrome.base(),
                crate::webui::encode_component(name)
            ))
        }
        Err(error) => match error.downcast_ref::<data::Refusal>() {
            Some(data::Refusal::Invalid { message, .. }) => (StatusCode::BAD_REQUEST, *message),
            Some(data::Refusal::Denied) => (
                StatusCode::FORBIDDEN,
                "Organization policy does not allow editing this property.",
            ),
            Some(data::Refusal::Missing) => (
                StatusCode::CONFLICT,
                "The repository property changed. Reload before retrying.",
            ),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Repository properties are unavailable.",
            ),
        },
    };
    let mut settings = match repository::settings_model(ctx, loaded, "properties").await {
        Ok(settings) => settings,
        Err(response) => return response,
    };
    let mut properties = match panel(ctx, loaded, Some(name)).await {
        Ok(properties) => properties,
        Err(response) => return response,
    };
    if action == "repository-property-set" {
        if let Some(property) = properties.selected.as_mut() {
            property.draft = raw.to_owned();
        }
    }
    settings.properties = Some(Box::new(properties));
    settings.error = Some(message.into());
    repository::settings_response(ctx, loaded, settings, status)
}
