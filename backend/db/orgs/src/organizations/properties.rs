use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::models::Repository;
use codeyard_db_core::native_operation;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::DbPool;
use std::collections::HashSet;

pub mod query;

pub const MAX_DEFINITIONS: usize = 100;
/// Leaves room for selected values, drafts and repository chrome in the 16 MiB page model.
pub const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;

pub const MAX_FORM_VALUE_BYTES: usize =
    MAX_ALLOWED_VALUES * MAX_VALUE_BYTES + 2 * (MAX_ALLOWED_VALUES - 1);

/// The catalog columns an overview shows, without the allowed values and default.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct DefinitionSummary {
    pub id: i64,
    pub property_name: String,
    pub value_type: String,
    pub description: Option<String>,
    pub required: bool,
    pub require_explicit_values: bool,
}

pub(crate) fn checked_changes(
    definitions: &[Definition],
    values: &[ValueInput],
) -> Result<Vec<(i64, Option<String>)>> {
    ensure!(
        values.len() <= MAX_DEFINITIONS,
        invalid("properties", "Too many repository properties")
    );
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for input in values {
        ensure!(
            seen.insert(input.property_name.to_ascii_lowercase()),
            invalid("properties", "Specify each property only once")
        );
        let definition = definitions
            .iter()
            .find(|definition| {
                definition
                    .fields
                    .property_name
                    .eq_ignore_ascii_case(&input.property_name)
            })
            .ok_or_else(|| invalid("property_name", "Unknown organization repository property"))?;
        if let Some(value) = &input.value {
            definition.fields.validate_value(value)?;
        } else {
            ensure!(
                !definition.fields.needs_explicit_value(),
                invalid(
                    "value",
                    "This repository property requires an explicit value"
                )
            );
        }
        result.push((
            definition.id,
            input
                .value
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ));
    }
    result.sort_unstable_by_key(|(id, _)| *id);
    Ok(result)
}
pub fn checked_catalog_bytes<'a>(
    fields: impl IntoIterator<Item = &'a DefinitionInput>,
) -> Result<()> {
    let mut bytes = 2usize;
    for definition in fields {
        bytes = bytes
            .checked_add(serde_json::to_vec(definition)?.len() + 1)
            .filter(|bytes| *bytes <= MAX_CATALOG_BYTES)
            .ok_or_else(|| {
                invalid(
                    "properties",
                    "The serialized property catalog must fit within 2 MiB",
                )
            })?;
    }
    Ok(())
}

pub(crate) fn checked_definitions(
    inputs: &[DefinitionInput],
    existing: &[Definition],
) -> Result<()> {
    ensure!(
        !inputs.is_empty() && inputs.len() <= MAX_DEFINITIONS,
        invalid(
            "properties",
            "Provide between 1 and 100 property definitions"
        )
    );
    let mut seen = HashSet::new();
    let mut names = existing
        .iter()
        .map(|definition| definition.fields.property_name.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    for input in inputs {
        input.validate()?;
        let name = input.property_name.to_ascii_lowercase();
        ensure!(
            seen.insert(name.clone()),
            invalid("properties", "Specify each property only once")
        );
        names.insert(name);
    }
    ensure!(
        names.len() <= MAX_DEFINITIONS,
        invalid(
            "properties",
            "An organization can define at most 100 repository properties"
        )
    );
    checked_catalog_bytes(
        existing
            .iter()
            .filter(|definition| {
                !seen.contains(&definition.fields.property_name.to_ascii_lowercase())
            })
            .map(|definition| &definition.fields)
            .chain(inputs.iter()),
    )
}
#[derive(Debug)]
pub struct RepositoryValues {
    pub repository: Repository,
    pub properties: Vec<ValueInput>,
}
#[derive(Debug)]
pub struct RepositoryMetadata {
    pub values: RepositoryValues,
    pub definitions: Vec<Definition>,
    pub organization_owner: bool,
    pub repository_admin: bool,
}

native_operation!(crate::postgres::organizations::properties::catalog,crate::sqlite::organizations::properties::catalog,
    fn catalog(pool:&DbPool,actor:i64,org:i64)->Result<OrganizationOutcome<Vec<Definition>>>);
native_operation!(crate::postgres::organizations::properties::summaries,crate::sqlite::organizations::properties::summaries,
    fn summaries(pool:&DbPool,actor:i64,org:i64)->Result<OrganizationOutcome<Vec<DefinitionSummary>>>);
native_operation!(codeyard_db_core::queries::postgres::organizations::properties::definition,codeyard_db_core::queries::sqlite::organizations::properties::definition,
    fn definition(pool:&DbPool,actor:i64,org:i64,name:&str)->Result<OrganizationOutcome<Option<Definition>>>);
native_operation!(crate::postgres::organizations::properties::define,crate::sqlite::organizations::properties::define,
    fn define(pool:&DbPool,actor:impl Into<codeyard_db_core::queries::credentials::Actor>,org:i64,inputs:&[DefinitionInput])->Result<OrganizationOutcome<Vec<Definition>>>);
native_operation!(crate::postgres::organizations::properties::remove,crate::sqlite::organizations::properties::remove,
    fn remove(pool:&DbPool,actor:impl Into<codeyard_db_core::queries::credentials::Actor>,org:i64,name:&str,definition:Option<i64>)->Result<OrganizationOutcome<()>>);
native_operation!(crate::postgres::organizations::properties::list_values,crate::sqlite::organizations::properties::list_values,
    fn list_values(pool:&DbPool,actor:i64,org:i64,query:&query::RepositoryQuery,window:(i64,i64))->Result<OrganizationOutcome<Vec<RepositoryValues>>>);
native_operation!(crate::postgres::organizations::properties::set_values,crate::sqlite::organizations::properties::set_values,
    fn set_values(pool:&DbPool,actor:impl Into<codeyard_db_core::queries::credentials::Actor>,org:i64,repositories:&[String],values:&[ValueInput])->Result<OrganizationOutcome<()>>);
native_operation!(crate::postgres::organizations::properties::repository,crate::sqlite::organizations::properties::repository,
    fn repository(pool:&DbPool,viewer:Option<i64>,repo:i64)->Result<Option<RepositoryMetadata>>);
native_operation!(crate::postgres::organizations::properties::set_repository_values,crate::sqlite::organizations::properties::set_repository_values,
    fn set_repository_values(pool:&DbPool,actor:impl Into<codeyard_db_core::queries::credentials::Actor>,repo:i64,values:&[ValueInput])->Result<()>);

native_operation!(crate::postgres::organizations::properties::set_repository_value,crate::sqlite::organizations::properties::set_repository_value,
    fn set_repository_value(pool:&DbPool,actor:impl Into<codeyard_db_core::queries::credentials::Actor>,repo:i64,definition:i64,value:&ValueInput)->Result<()>);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn orgs_property_values_distinguish_unset_from_missing_and_boolean_strings() {
        assert!(serde_json::from_value::<ValueInput>(json!({"property_name":"env"})).is_err());
        assert!(
            serde_json::from_value::<ValueInput>(json!({"property_name":"env","value":true}))
                .is_err()
        );
        assert_eq!(
            serde_json::from_value::<ValueInput>(json!({"property_name":"env","value":null}))
                .unwrap()
                .value,
            None
        );
        let boolean:DefinitionInput=serde_json::from_value(json!({"property_name":"enabled","value_type":"true_false","default_value":"false","required":true,"values_editable_by":null})).unwrap();
        boolean.validate().unwrap();
        assert_eq!(boolean.values_editable_by, EditableBy::OrgActors);
        assert!(boolean
            .validate_value(&PropertyValue::String("TRUE".into()))
            .is_err());
    }
    #[test]
    fn orgs_property_types_validate_choices_defaults_and_urls() {
        let mut definition:DefinitionInput=serde_json::from_value(json!({"property_name":"tags","value_type":"multi_select","allowed_values":["one","two"],"default_value":["one"]})).unwrap();
        definition.validate().unwrap();
        assert!(definition
            .validate_value(&PropertyValue::String("one".into()))
            .is_err());
        assert!(definition
            .validate_value(&PropertyValue::Strings(vec!["one".into(), "one".into()]))
            .is_err());
        definition.required = true;
        assert!(definition
            .validate_value(&PropertyValue::Strings(vec![]))
            .is_err());
        definition.allowed_values = Some(vec!["two".into()]);
        assert!(definition.validate().is_err());
        let url: DefinitionInput =
            serde_json::from_value(json!({"property_name":"docs","value_type":"url"})).unwrap();
        assert!(url
            .validate_value(&PropertyValue::String("https://example.org/docs".into()))
            .is_ok());
        assert!(url
            .validate_value(&PropertyValue::String("javascript:alert(1)".into()))
            .is_err());
        assert!(url
            .validate_value(&PropertyValue::String("/relative".into()))
            .is_err());
    }
    #[test]
    fn orgs_property_names_and_definition_batches_are_unambiguous() {
        for name in ["", "slash/name", "non ascii é", "newline\n"] {
            assert!(!valid_name(name));
        }
        let first: DefinitionInput =
            serde_json::from_value(json!({"property_name":"Env","value_type":"string"})).unwrap();
        let mut alias = first.clone();
        alias.property_name = "env".into();
        assert!(checked_definitions(&[first, alias], &[]).is_err());
    }
}

use codeyard_db_core::queries::organizations::properties::ValueInput;

use codeyard_db_core::queries::organizations::properties::DefinitionInput;

use codeyard_db_core::queries::organizations::properties::Definition;

use codeyard_db_core::queries::organizations::properties::invalid;

use codeyard_db_core::queries::organizations::properties::MAX_VALUE_BYTES;

use codeyard_db_core::queries::organizations::properties::MAX_ALLOWED_VALUES;
