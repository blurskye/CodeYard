//! External repository values and installation labels belong to one organization.
use super::OrganizationOutcome;
use crate::backend::db::DbPool;
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(sqlx::FromRow)]
pub struct Installation {
    pub display_name: String,
    pub installation_id: i64,
}

pub struct RepositoryValue {
    pub repository_name: String,
    pub property_name: String,
    pub value: Value,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.chars().any(char::is_control)
}
pub fn valid_value(value: &Value) -> bool {
    value.is_null()
        || value.as_str().is_some_and(|value| value.len() <= 4096)
        || value.as_array().is_some_and(|values| {
            values.len() <= 100
                && values
                    .iter()
                    .all(|value| value.as_str().is_some_and(|value| value.len() <= 4096))
        })
}

/// The last value given for each repository and property, split into values to
/// store and values to remove (a null value unsets the property).
pub(crate) struct Changes<'a> {
    pub repositories: Vec<&'a str>,
    pub sets: Vec<(&'a str, &'a str, String)>,
    pub unsets: Vec<(&'a str, &'a str)>,
}

/// Validates every value and groups them by repository, or returns `None` when one is invalid.
pub(crate) fn changes(values: &[RepositoryValue]) -> Result<Option<Changes<'_>>> {
    let mut latest = BTreeMap::new();
    for value in values {
        if !valid_name(&value.property_name) || !valid_value(&value.value) {
            return Ok(None);
        }
        latest.insert(
            (value.repository_name.as_str(), value.property_name.as_str()),
            &value.value,
        );
    }
    let mut changes = Changes {
        repositories: Vec::new(),
        sets: Vec::new(),
        unsets: Vec::new(),
    };
    for ((repository, property), value) in latest {
        if changes.repositories.last() != Some(&repository) {
            changes.repositories.push(repository);
        }
        if value.is_null() {
            changes.unsets.push((repository, property));
        } else {
            changes
                .sets
                .push((repository, property, serde_json::to_string(value)?));
        }
    }
    Ok(Some(changes))
}

native_operation!(crate::backend::db::postgres::organizations::external_properties::installations,
    crate::backend::db::sqlite::organizations::external_properties::installations,
    fn installations(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<Vec<Installation>>>);
native_operation!(crate::backend::db::postgres::organizations::external_properties::register,
    crate::backend::db::sqlite::organizations::external_properties::register,
    fn register(pool: &DbPool, actor: i64, org: i64, name: &str, installation: Option<i64>) -> Result<OrganizationOutcome<Installation>>);
native_operation!(crate::backend::db::postgres::organizations::external_properties::definitions,
    crate::backend::db::sqlite::organizations::external_properties::definitions,
    fn definitions(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<Vec<String>>>);
native_operation!(crate::backend::db::postgres::organizations::external_properties::set_values,
    crate::backend::db::sqlite::organizations::external_properties::set_values,
    fn set_values(pool: &DbPool, actor: i64, org: i64, values: &[RepositoryValue]) -> Result<OrganizationOutcome<()>>);
native_operation!(crate::backend::db::postgres::organizations::external_properties::delete_values,
    crate::backend::db::sqlite::organizations::external_properties::delete_values,
    fn delete_values(pool: &DbPool, actor: i64, org: i64, property: &str) -> Result<OrganizationOutcome<()>>);
