//! Bounded native creation drafts; definitions and write authority remain server-side.
use super::{CreationRefusal, NewRepositoryForm, PageContext, RepositoryCreationChoices};
use crate::{
    backend::db::{
        models::OrgRole,
        organizations::{self as orgs, properties as data, OrganizationOutcome::Applied},
    },
    webui::components::repository_properties::RepositoryPropertyField,
};
use axum::http::StatusCode;
use std::collections::HashMap;

pub(super) fn drafts(
    form: &HashMap<String, String>,
    values: &mut NewRepositoryForm,
) -> Result<(), CreationRefusal> {
    values.properties_owner_org_id = super::draft_value(form, "properties_owner_org_id", 32);
    for (key, value) in form {
        if let Some(name) = key
            .strip_prefix("property_value_")
            .filter(|name| data::valid_name(name))
        {
            if values.property_values.len() < data::MAX_DEFINITIONS {
                values
                    .property_values
                    .insert(name.to_owned(), super::draft_value(form, key, data::MAX_FORM_VALUE_BYTES));
            }
        }
        if let Some(name) = key
            .strip_prefix("property_set_")
            .filter(|name| data::valid_name(name))
        {
            if values.property_set.len() < data::MAX_DEFINITIONS
                && matches!(value.as_str(), "true" | "on")
            {
                values.property_set.insert(name.to_owned());
            }
        }
    }
    let mut count = 0;
    for (key, value) in form {
        if let Some(name) = key.strip_prefix("property_value_") {
            count += 1;
            if !data::valid_name(name) || value.len() > data::MAX_FORM_VALUE_BYTES {
                return Err((None, "Invalid repository property input."));
            }
        }
        if let Some(name) = key.strip_prefix("property_set_") {
            if !data::valid_name(name) || !matches!(value.as_str(), "true" | "on") {
                return Err((None, "Invalid repository property selection."));
            }
        }
    }
    if count > data::MAX_DEFINITIONS
        || form
            .keys()
            .filter(|key| key.starts_with("property_set_"))
            .count()
            > data::MAX_DEFINITIONS
    {
        return Err((None, "Too many repository properties."));
    }
    super::selected_organization(
        form.get("properties_owner_org_id")
            .map(String::as_str)
            .unwrap_or(""),
    )
    .map_err(|message| (None, message))?;
    Ok(())
}
pub(super) async fn populate(
    ctx: &PageContext,
    choices: &RepositoryCreationChoices,
    form: &mut NewRepositoryForm,
) -> Result<(), StatusCode> {
    form.property_fields.clear();
    let Ok(target) = super::selected_organization(&form.owner_org_id) else {
        return Ok(());
    };
    let source = super::selected_organization(&form.properties_owner_org_id)
        .ok()
        .flatten();
    if target != source {
        form.property_values.clear();
        form.property_set.clear();
        form.properties_owner_org_id.clear();
    }
    let Some(org) = target.filter(|id| choices.organizations.iter().any(|row| row.id == *id))
    else {
        return Ok(());
    };
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let definitions = match data::catalog(&ctx.state.pool, actor, org).await {
        Ok(Applied(rows)) => rows,
        Ok(_) => return Ok(()),
        Err(_) => return Err(StatusCode::SERVICE_UNAVAILABLE),
    };
    data::checked_catalog_bytes(definitions.iter().map(|definition| &definition.fields))
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    let owner = matches!(
        orgs::get_organization_role(&ctx.state.pool, actor, org).await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?,
        Applied(OrgRole::Owner)
    );
    form.properties_owner_org_id = org.to_string();
    form.property_values.retain(|name, _| {
        definitions
            .iter()
            .any(|row| row.fields.property_name == *name)
    });
    form.property_set.retain(|name| {
        definitions
            .iter()
            .any(|row| row.fields.property_name == *name)
    });
    form.property_fields = definitions
        .iter()
        .map(|definition| {
            let fields = &definition.fields;
            RepositoryPropertyField {
                name: fields.property_name.clone(),
                value_type: fields.value_type.as_str().into(),
                description: fields.description.clone().unwrap_or_default(),
                allowed_values: fields.allowed_values.clone().unwrap_or_default(),
                required: fields.needs_explicit_value(),
                editable: owner || fields.values_editable_by == data::EditableBy::OrgAndRepoActors,
                default_preview: fields
                    .default_value
                    .as_ref()
                    .map(|value| crate::webui::excerpt(&value.form_value(), 256)),
            }
        })
        .collect();
    Ok(())
}
pub(super) fn failure_message(status: StatusCode) -> &'static str {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        "The property catalog exceeds the native form limit. Ask an organization owner to reduce it before continuing."
    } else {
        "Repository properties are temporarily unavailable."
    }
}

pub(super) async fn values(
    ctx: &PageContext,
    actor: i64,
    target: Option<i64>,
    form: &NewRepositoryForm,
) -> Result<Vec<data::ValueInput>, (StatusCode, &'static str)> {
    let invalid = |message| (StatusCode::BAD_REQUEST, message);
    let source = super::selected_organization(&form.properties_owner_org_id).map_err(invalid)?;
    if source != target
        && (source.is_some() || !form.property_values.is_empty() || !form.property_set.is_empty())
    {
        return Err(invalid(
            "Load properties for the selected owner before creating the repository.",
        ));
    }
    if form.property_set.is_empty() {
        return Ok(Vec::new());
    }
    let org = target
        .ok_or_else(|| invalid("Personal repositories cannot have organization properties."))?;
    let definitions = match data::catalog(&ctx.state.pool, actor, org).await {
        Ok(Applied(rows)) => rows,
        Ok(_) => return Err((StatusCode::FORBIDDEN, "Repository owner is unavailable.")),
        Err(_) => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "Repository properties are temporarily unavailable.",
            ))
        }
    };
    let mut names: Vec<_> = form.property_set.iter().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let definition = definitions
                .iter()
                .find(|row| row.fields.property_name.eq_ignore_ascii_case(name))
                .ok_or_else(|| invalid("Unknown repository property."))?;
            let value = form
                .property_values
                .get(name)
                .ok_or_else(|| invalid("Provide a value for each selected property."))?;
            Ok(data::ValueInput {
                property_name: name.clone(),
                value: Some(data::PropertyValue::from_form(
                    definition.fields.value_type,
                    value,
                )),
            })
        })
        .collect()
}
