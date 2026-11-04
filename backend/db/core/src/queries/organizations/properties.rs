use crate::models::Repository;
use anyhow::ensure;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
pub const MAX_ALLOWED_VALUES: usize = 200;

pub const MAX_VALUE_BYTES: usize = 1024;

pub fn effective_values(
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
    pub fn same_value_rules(&self, other: &Self) -> bool {
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
pub struct DefinitionRow {
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
    pub fn decode(self) -> Result<Definition> {
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
