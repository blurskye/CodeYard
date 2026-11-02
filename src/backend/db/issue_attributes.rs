//! Owner-inherited issue types and organization custom fields.
use super::{credentials::Actor, DbPool};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct SelectOption {
    pub id: i64,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
    pub priority: i64,
}
/// The type of one issue in a batch read.
#[derive(sqlx::FromRow)]
pub struct AssignedType {
    pub issue_id: i64,
    #[sqlx(flatten)]
    pub attribute: Attribute,
}
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Attribute {
    pub id: i64,
    pub owner_user_id: Option<i64>,
    pub owner_org_id: Option<i64>,
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    pub data_type: Option<String>,
    pub visibility: String,
    pub is_enabled: bool,
    pub color: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[sqlx(skip)]
    pub options: Vec<SelectOption>,
}
#[derive(Serialize, Deserialize)]
pub struct FieldValue {
    pub id: i64,
    pub attribute: Attribute,
    pub value: Value,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueInput {
    pub field_id: i64,
    pub value: Value,
}
#[derive(Clone, Deserialize)]
pub struct OptionInput {
    pub id: Option<i64>,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
    pub priority: Option<i64>,
}
#[derive(Default)]
pub struct FieldChanges {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub visibility: Option<String>,
    pub options: Option<Vec<OptionInput>>,
}
impl FieldChanges {
    pub(crate) fn apply_to(&self, current: &Attribute) -> Definition {
        Definition {
            id: Some(current.id),
            expected_updated: Some(current.updated_at),
            kind: "field".into(),
            name: self.name.clone().unwrap_or_else(|| current.name.clone()),
            description: self
                .description
                .clone()
                .unwrap_or_else(|| current.description.clone()),
            data_type: current.data_type.clone(),
            visibility: self
                .visibility
                .clone()
                .unwrap_or_else(|| current.visibility.clone()),
            is_enabled: current.is_enabled,
            color: current.color.clone(),
            options: self.options.clone().unwrap_or_else(|| {
                current
                    .options
                    .iter()
                    .map(|option| OptionInput {
                        id: Some(option.id),
                        name: option.name.clone(),
                        color: option.color.clone(),
                        description: option.description.clone(),
                        priority: Some(option.priority),
                    })
                    .collect()
            }),
        }
    }
}
pub fn owner_key(user: Option<i64>, org: Option<i64>) -> String {
    org.map_or_else(
        || format!("user:{}", user.unwrap_or(0)),
        |id| format!("org:{id}"),
    )
}

pub fn revision(attribute: &Attribute) -> Result<String> {
    use sha2::Digest;
    Ok(format!(
        "{:x}",
        sha2::Sha256::digest(serde_json::to_vec(attribute)?)
    ))
}

pub struct Definition {
    pub id: Option<i64>,
    pub expected_updated: Option<DateTime<Utc>>,
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    pub data_type: Option<String>,
    pub visibility: String,
    pub is_enabled: bool,
    pub color: Option<String>,
    pub options: Vec<OptionInput>,
}
pub const COLORS: [&str; 8] = [
    "gray", "blue", "green", "yellow", "orange", "red", "pink", "purple",
];
fn valid_text(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}
impl Definition {
    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            valid_text(&self.name, 128)
                && self
                    .description
                    .as_ref()
                    .is_none_or(|d| d.len() <= 1024 && !d.contains('\0')),
            Refusal::Invalid
        );
        // An issue type's timeline snapshot holds its escaped name, id and color in 255 characters.
        anyhow::ensure!(
            self.kind != "type" || serde_json::to_string(&self.name)?.chars().count() <= 200,
            Refusal::Invalid
        );
        anyhow::ensure!(
            self.color
                .as_ref()
                .is_none_or(|c| COLORS.contains(&c.as_str())),
            Refusal::Invalid
        );
        if self.kind == "type" {
            anyhow::ensure!(
                self.data_type.is_none() && self.options.is_empty(),
                Refusal::Invalid
            );
        } else {
            anyhow::ensure!(
                self.kind == "field"
                    && matches!(
                        self.data_type.as_deref(),
                        Some("text" | "number" | "date" | "single_select" | "multi_select")
                    )
                    && matches!(
                        self.visibility.as_str(),
                        "all" | "organization_members_only"
                    ),
                Refusal::Invalid
            );
            let select = matches!(
                self.data_type.as_deref(),
                Some("single_select" | "multi_select")
            );
            anyhow::ensure!(
                (select && self.options.len() <= 100) || (!select && self.options.is_empty()),
                Refusal::Invalid
            );
            let mut names = std::collections::HashSet::new();
            let mut ids = std::collections::HashSet::new();
            for option in &self.options {
                anyhow::ensure!(
                    valid_text(&option.name, 128)
                        && COLORS.contains(&option.color.as_str())
                        && option
                            .description
                            .as_ref()
                            .is_none_or(|text| text.len() <= 1024 && !text.contains('\0'))
                        && names.insert(option.name.to_lowercase())
                        && option.id.is_none_or(|id| id > 0 && ids.insert(id)),
                    Refusal::Invalid
                );
            }
        }
        Ok(())
    }
}
impl Attribute {
    fn option_id(&self, value: &Value) -> Result<i64> {
        self.options
            .iter()
            .find(|option| {
                value.as_i64() == Some(option.id) || value.as_str() == Some(option.name.as_str())
            })
            .map(|option| option.id)
            .ok_or_else(|| Refusal::Invalid.into())
    }

    /// Select values are stored as stable option IDs, never mutable option names.
    pub(crate) fn canonical(&self, value: &Value) -> Result<Value> {
        let invalid = || Refusal::Invalid;
        Ok(match self.data_type.as_deref() {
            Some("text") => {
                let text = value
                    .as_str()
                    .filter(|s| s.len() <= 65536 && !s.contains('\0'))
                    .ok_or_else(invalid)?;
                Value::String(text.into())
            }
            Some("number") => {
                anyhow::ensure!(value.is_number(), Refusal::Invalid);
                value.clone()
            }
            Some("date") => {
                let text = value.as_str().ok_or_else(invalid)?;
                anyhow::ensure!(
                    crate::backend::issues::calendar_day(text).is_some(),
                    Refusal::Invalid
                );
                value.clone()
            }
            Some("single_select") => self.option_id(value)?.into(),
            Some("multi_select") => {
                let values = value
                    .as_array()
                    .filter(|v| v.len() <= self.options.len())
                    .ok_or_else(invalid)?;
                let mut result = Vec::new();
                for value in values {
                    let id = self.option_id(value)?;
                    anyhow::ensure!(!result.contains(&Value::from(id)), Refusal::Invalid);
                    result.push(id.into());
                }
                Value::Array(result)
            }
            _ => return Err(Refusal::Invalid.into()),
        })
    }
}
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Issue attribute mutation denied")]
    Denied,
    #[error("Issue or attribute not found")]
    Missing,
    #[error("Invalid issue attribute or value")]
    Invalid,
    #[error("Issue attribute limit exceeded")]
    Limit,
    #[error("This definition changed; reload it before saving again")]
    Conflict,
}
native_operation!(crate::backend::db::postgres::issue_attributes::catalog,crate::backend::db::sqlite::issue_attributes::catalog,
    fn catalog(pool:&DbPool,repo:i64,viewer:Option<i64>,kind:&str)->Result<Vec<Attribute>>);
native_operation!(crate::backend::db::postgres::issue_attributes::organization_catalog,crate::backend::db::sqlite::issue_attributes::organization_catalog,
    fn organization_catalog(pool:&DbPool,org:i64,actor:i64,kind:&str)->Result<Vec<Attribute>>);
native_operation!(crate::backend::db::postgres::issue_attributes::configure_organization,crate::backend::db::sqlite::issue_attributes::configure_organization,
    fn configure_organization(pool:&DbPool,org:i64,actor:impl Into<Actor>,definition:&Definition)->Result<Attribute>);
native_operation!(crate::backend::db::postgres::issue_attributes::create_organization_field,crate::backend::db::sqlite::issue_attributes::create_organization_field,
    fn create_organization_field(pool:&DbPool,org:i64,actor:impl Into<Actor>,definition:&Definition)->Result<Attribute>);
native_operation!(crate::backend::db::postgres::issue_attributes::patch_organization_field,crate::backend::db::sqlite::issue_attributes::patch_organization_field,
    fn patch_organization_field(pool:&DbPool,org:i64,actor:impl Into<Actor>,id:i64,changes:&FieldChanges)->Result<Attribute>);
native_operation!(crate::backend::db::postgres::issue_attributes::delete_organization_definition,crate::backend::db::sqlite::issue_attributes::delete_organization_definition,
    fn delete_organization_definition(pool:&DbPool,org:i64,actor:impl Into<Actor>,id:i64,kind:&str)->Result<()>);
native_operation!(crate::backend::db::postgres::issue_attributes::values,crate::backend::db::sqlite::issue_attributes::values,
    fn values(pool:&DbPool,repo:i64,issue:i64,viewer:Option<i64>)->Result<Vec<FieldValue>>);
native_operation!(crate::backend::db::postgres::issue_attributes::assigned_type,crate::backend::db::sqlite::issue_attributes::assigned_type,
    fn assigned_type(pool:&DbPool,repo:i64,issue:i64)->Result<Option<Attribute>>);
native_operation!(crate::backend::db::postgres::issue_attributes::assigned_types,crate::backend::db::sqlite::issue_attributes::assigned_types,
    /// The types of the repository's issues among `ids` (at most 500).
    fn assigned_types(pool:&DbPool,repo:i64,ids:&[i64])->Result<Vec<AssignedType>>);
native_operation!(crate::backend::db::postgres::issue_attributes::owner_can_configure,crate::backend::db::sqlite::issue_attributes::owner_can_configure,
    fn owner_can_configure(pool:&DbPool,repo:i64,actor:i64)->Result<bool>);
native_operation!(crate::backend::db::postgres::issue_attributes::configure,crate::backend::db::sqlite::issue_attributes::configure,
    fn configure(pool:&DbPool,repo:i64,actor:impl Into<Actor>,definition:&Definition)->Result<i64>);
native_operation!(crate::backend::db::postgres::issue_attributes::configure_checked,crate::backend::db::sqlite::issue_attributes::configure_checked,
    fn configure_checked(pool:&DbPool,repo:i64,actor:impl Into<Actor>,definition:&Definition,guard:(Option<&str>,Option<&str>))->Result<i64>);
native_operation!(crate::backend::db::postgres::issue_attributes::delete_definition,crate::backend::db::sqlite::issue_attributes::delete_definition,
    fn delete_definition(pool:&DbPool,repo:i64,actor:impl Into<Actor>,id:i64)->Result<()>);
native_operation!(crate::backend::db::postgres::issue_attributes::set_values,crate::backend::db::sqlite::issue_attributes::set_values,
    fn set_values(pool:&DbPool,repo:i64,actor:impl Into<Actor>,issue:i64,values:&[ValueInput],replace:bool)->Result<()>);
native_operation!(crate::backend::db::postgres::issue_attributes::delete_value,crate::backend::db::sqlite::issue_attributes::delete_value,
    fn delete_value(pool:&DbPool,repo:i64,actor:impl Into<Actor>,issue:i64,field:i64)->Result<()>);
native_operation!(crate::backend::db::postgres::issue_attributes::set_type,crate::backend::db::sqlite::issue_attributes::set_type,
    fn set_type(pool:&DbPool,repo:i64,actor:impl Into<Actor>,issue:i64,kind:Option<i64>)->Result<()>);
