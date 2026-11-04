//! Organization-defined repository metadata. Definition authority and repository value
//! authority remain separate; repository administrators cannot edit organization-only values.
use crate::backend::db::organizations::OrganizationOutcome;
use anyhow::{ensure, Result};
use codeyard_db_core::models::Repository;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub mod query;

pub const MAX_DEFINITIONS: usize = 100;
/// Leaves room for selected values, drafts and repository chrome in the 16 MiB page model.
pub const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ALLOWED_VALUES: usize = 200;
pub const MAX_VALUE_BYTES: usize = 1024;
pub const MAX_FORM_VALUE_BYTES: usize =
    MAX_ALLOWED_VALUES * MAX_VALUE_BYTES + 2 * (MAX_ALLOWED_VALUES - 1);

pub(crate) fn effective_values(
    definitions: &[Definition],
    rows: &std::collections::HashMap<i64, String>,
) -> Result<Vec<ValueInput>> {
    let mut result = Vec::new();
    let mut bytes = 0;
    for definition in definitions {
        let value = match rows.get(&definition.id) {
            Some(value) => Some(serde_json::from_str::<PropertyValue>(value)?),
            None if definition.fields.require_explicit_values => None,
            None => definition.fields.default_value.clone(),
        };
        if let Some(value) = &value {
            definition.fields.validate_value(value)?;
        }
        bytes += serde_json::to_string(&value)?.len() + definition.fields.property_name.len() + 64;
        ensure!(bytes <= 64 * 1024 * 1024, Refusal::TooLarge);
        result.push(ValueInput {
            property_name: definition.fields.property_name.clone(),
            value,
        });
    }
    Ok(result)
}

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("{message}")]
    Invalid {
        field: &'static str,
        message: &'static str,
    },
    #[error("Repository property access denied")]
    Denied,
    #[error("Repository property not found")]
    Missing,
    #[error("Repository property response is too large; request a smaller page")]
    TooLarge,
}
pub fn invalid(field: &'static str, message: &'static str) -> Refusal {
    Refusal::Invalid { field, message }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueType {
    String,
    SingleSelect,
    MultiSelect,
    TrueFalse,
    Url,
}
impl ValueType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::SingleSelect => "single_select",
            Self::MultiSelect => "multi_select",
            Self::TrueFalse => "true_false",
            Self::Url => "url",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "string" => Self::String,
            "single_select" => Self::SingleSelect,
            "multi_select" => Self::MultiSelect,
            "true_false" => Self::TrueFalse,
            "url" => Self::Url,
            _ => return Err(invalid("value_type", "Invalid repository property type").into()),
        })
    }
    pub fn selects(self) -> bool {
        matches!(self, Self::SingleSelect | Self::MultiSelect)
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditableBy {
    #[default]
    OrgActors,
    OrgAndRepoActors,
}
impl EditableBy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OrgActors => "org_actors",
            Self::OrgAndRepoActors => "org_and_repo_actors",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "org_actors" => Self::OrgActors,
            "org_and_repo_actors" => Self::OrgAndRepoActors,
            _ => {
                return Err(invalid(
                    "values_editable_by",
                    "Invalid repository property editor policy",
                )
                .into())
            }
        })
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PropertyValue {
    String(String),
    Strings(Vec<String>),
}
impl PropertyValue {
    pub fn present(&self) -> bool {
        match self {
            Self::String(value) => !value.trim().is_empty(),
            Self::Strings(values) => !values.is_empty(),
        }
    }
    pub fn from_form(kind: ValueType, value: &str) -> Self {
        if kind == ValueType::MultiSelect {
            Self::Strings(
                value
                    .lines()
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect(),
            )
        } else {
            Self::String(value.to_owned())
        }
    }
    pub fn form_value(&self) -> String {
        match self {
            Self::String(value) => value.clone(),
            Self::Strings(values) => values.join("\n"),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DefinitionInput {
    pub property_name: String,
    pub value_type: ValueType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default_value: Option<PropertyValue>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub allowed_values: Option<Vec<String>>,
    #[serde(default, deserialize_with = "editor_policy")]
    pub values_editable_by: EditableBy,
    #[serde(default)]
    pub require_explicit_values: bool,
}
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 75
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}
impl DefinitionInput {
    pub(crate) fn same_value_rules(&self, other: &Self) -> bool {
        self.value_type == other.value_type
            && self.allowed_values == other.allowed_values
            && self.required == other.required
            && self.require_explicit_values == other.require_explicit_values
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_name(&self.property_name),
            invalid(
                "property_name",
                "Property names must contain 1-75 ASCII letters, numbers, hyphens or underscores"
            )
        );
        ensure!(
            self.description
                .as_ref()
                .is_none_or(|value| value.len() <= 1024 && !value.contains('\0')),
            invalid(
                "description",
                "Property descriptions must contain at most 1024 UTF-8 bytes"
            )
        );
        if self.value_type.selects() {
            let values = self.allowed_values.as_deref().unwrap_or_default();
            ensure!(
                values.len() <= MAX_ALLOWED_VALUES,
                invalid(
                    "allowed_values",
                    "A property can have at most 200 allowed values"
                )
            );
            let mut seen = HashSet::new();
            for value in values {
                ensure!(!value.is_empty()&&value.len()<=MAX_VALUE_BYTES&&!value.chars().any(char::is_control)&&seen.insert(value),invalid("allowed_values","Allowed values must be distinct nonempty strings of at most 1024 UTF-8 bytes without control characters"));
            }
        } else {
            ensure!(
                self.allowed_values.is_none(),
                invalid(
                    "allowed_values",
                    "Only select properties can have allowed values"
                )
            );
        }
        if let Some(value) = &self.default_value {
            self.validate_value(value).map_err(|_| {
                invalid(
                    "default_value",
                    "The default must match the property's type, required state and allowed values",
                )
            })?;
        }
        Ok(())
    }
    pub fn validate_value(&self, value: &PropertyValue) -> Result<()> {
        let allowed = self.allowed_values.as_deref().unwrap_or_default();
        match (&self.value_type, value) {
            (ValueType::MultiSelect, PropertyValue::Strings(values)) => {
                ensure!(
                    values.len() <= MAX_ALLOWED_VALUES,
                    invalid("value", "Too many selected property values")
                );
                let mut seen = HashSet::new();
                ensure!(
                    values
                        .iter()
                        .all(|value| allowed.contains(value) && seen.insert(value)),
                    invalid(
                        "value",
                        "Select distinct values from the property's allowed values"
                    )
                );
            }
            (ValueType::MultiSelect, _) | (_, PropertyValue::Strings(_)) => {
                return Err(invalid("value", "The property value has the wrong type").into())
            }
            (kind, PropertyValue::String(value)) => {
                ensure!(value.len()<=MAX_VALUE_BYTES&&!value.contains('\0'),invalid("value","Property values must contain at most 1024 UTF-8 bytes and no NUL characters"));
                match kind {
                    ValueType::SingleSelect => ensure!(
                        allowed.contains(value),
                        invalid("value", "Select one of the property's allowed values")
                    ),
                    ValueType::TrueFalse => ensure!(
                        matches!(value.as_str(), "true" | "false"),
                        invalid(
                            "value",
                            "Boolean property values must be the strings true or false"
                        )
                    ),
                    ValueType::Url => ensure!(
                        url::Url::parse(value).ok().is_some_and(|url| matches!(
                            url.scheme(),
                            "http" | "https"
                        ) && url
                            .host_str()
                            .is_some())
                            && !value.chars().any(char::is_control),
                        invalid(
                            "value",
                            "URL properties require an absolute HTTP or HTTPS URL"
                        )
                    ),
                    _ => {}
                }
            }
        }
        ensure!(
            !(self.required || self.require_explicit_values) || value.present(),
            invalid("value", "A required repository property cannot be empty")
        );
        Ok(())
    }
    pub fn needs_explicit_value(&self) -> bool {
        self.require_explicit_values || (self.required && self.default_value.is_none())
    }
}
#[derive(Clone, Debug)]
pub struct Definition {
    pub id: i64,
    pub org_id: i64,
    pub fields: DefinitionInput,
}
#[derive(sqlx::FromRow)]
pub(crate) struct DefinitionRow {
    pub id: i64,
    pub org_id: i64,
    pub property_name: String,
    pub value_type: String,
    pub required: bool,
    pub default_value: Option<String>,
    pub description: Option<String>,
    pub allowed_values: Option<String>,
    pub values_editable_by: String,
    pub require_explicit_values: bool,
}
impl DefinitionRow {
    pub(crate) fn decode(self) -> Result<Definition> {
        let fields = DefinitionInput {
            property_name: self.property_name,
            value_type: ValueType::parse(&self.value_type)?,
            required: self.required,
            default_value: self
                .default_value
                .map(|value| serde_json::from_str(&value))
                .transpose()?,
            description: self.description,
            allowed_values: self
                .allowed_values
                .map(|value| serde_json::from_str(&value))
                .transpose()?,
            values_editable_by: EditableBy::parse(&self.values_editable_by)?,
            require_explicit_values: self.require_explicit_values,
        };
        fields.validate()?;
        Ok(Definition {
            id: self.id,
            org_id: self.org_id,
            fields,
        })
    }
}
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValueInput {
    pub property_name: String,
    #[serde(deserialize_with = "nullable_value")]
    pub value: Option<PropertyValue>,
}
fn nullable_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<PropertyValue>, D::Error> {
    Option::<PropertyValue>::deserialize(deserializer)
}
fn editor_policy<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<EditableBy, D::Error> {
    Ok(Option::<EditableBy>::deserialize(deserializer)?.unwrap_or_default())
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
pub(crate) fn checked_catalog_bytes<'a>(
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

native_operation!(crate::backend::db::postgres::organizations::properties::catalog,crate::backend::db::sqlite::organizations::properties::catalog,
    fn catalog(pool:&DbPool,actor:i64,org:i64)->Result<OrganizationOutcome<Vec<Definition>>>);
native_operation!(crate::backend::db::postgres::organizations::properties::summaries,crate::backend::db::sqlite::organizations::properties::summaries,
    fn summaries(pool:&DbPool,actor:i64,org:i64)->Result<OrganizationOutcome<Vec<DefinitionSummary>>>);
native_operation!(crate::backend::db::postgres::organizations::properties::definition,crate::backend::db::sqlite::organizations::properties::definition,
    fn definition(pool:&DbPool,actor:i64,org:i64,name:&str)->Result<OrganizationOutcome<Option<Definition>>>);
native_operation!(crate::backend::db::postgres::organizations::properties::define,crate::backend::db::sqlite::organizations::properties::define,
    fn define(pool:&DbPool,actor:impl Into<crate::backend::db::credentials::Actor>,org:i64,inputs:&[DefinitionInput])->Result<OrganizationOutcome<Vec<Definition>>>);
native_operation!(crate::backend::db::postgres::organizations::properties::remove,crate::backend::db::sqlite::organizations::properties::remove,
    fn remove(pool:&DbPool,actor:impl Into<crate::backend::db::credentials::Actor>,org:i64,name:&str,definition:Option<i64>)->Result<OrganizationOutcome<()>>);
native_operation!(crate::backend::db::postgres::organizations::properties::list_values,crate::backend::db::sqlite::organizations::properties::list_values,
    fn list_values(pool:&DbPool,actor:i64,org:i64,query:&query::RepositoryQuery,window:(i64,i64))->Result<OrganizationOutcome<Vec<RepositoryValues>>>);
native_operation!(crate::backend::db::postgres::organizations::properties::set_values,crate::backend::db::sqlite::organizations::properties::set_values,
    fn set_values(pool:&DbPool,actor:impl Into<crate::backend::db::credentials::Actor>,org:i64,repositories:&[String],values:&[ValueInput])->Result<OrganizationOutcome<()>>);
native_operation!(crate::backend::db::postgres::organizations::properties::repository,crate::backend::db::sqlite::organizations::properties::repository,
    fn repository(pool:&DbPool,viewer:Option<i64>,repo:i64)->Result<Option<RepositoryMetadata>>);
native_operation!(crate::backend::db::postgres::organizations::properties::set_repository_values,crate::backend::db::sqlite::organizations::properties::set_repository_values,
    fn set_repository_values(pool:&DbPool,actor:impl Into<crate::backend::db::credentials::Actor>,repo:i64,values:&[ValueInput])->Result<()>);

native_operation!(crate::backend::db::postgres::organizations::properties::set_repository_value,crate::backend::db::sqlite::organizations::properties::set_repository_value,
    fn set_repository_value(pool:&DbPool,actor:impl Into<crate::backend::db::credentials::Actor>,repo:i64,definition:i64,value:&ValueInput)->Result<()>);

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
