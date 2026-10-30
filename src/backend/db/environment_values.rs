//! Environment values are delivered only to jobs targeting that environment.
pub use super::organization_values::Kind;
use super::DbPool;
use codeyard_actions::values::MAX_SCOPE_VALUES;
use crate::backend::crypto::{SecretEncryptor, SecretError};
use chrono::{DateTime, Utc};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct EnvironmentValue {
    pub name: String,
    pub value: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct Save<'a> {
    pub kind: Kind,
    pub previous: Option<&'a str>,
    pub name: &'a str,
    pub value: Option<&'a str>,
}

#[derive(Debug, thiserror::Error)]
pub enum ValueError {
    #[error("Environment database unavailable")]
    Database(#[from] sqlx::Error),
    #[error("Environment authority unavailable")]
    Authority(#[source] anyhow::Error),
    #[error("Environment secret encryption unavailable")]
    Encryption(#[from] SecretError),
    #[error("Environment access denied")]
    Denied,
    #[error("Environment or value not found")]
    NotFound,
    #[error("Environment variable already exists")]
    Exists,
    #[error("Environment names must be nonblank and contain 1 to 255 characters without control characters")]
    InvalidEnvironment,
    #[error("Value name is invalid or reserved")]
    InvalidName,
    #[error("Value exceeds 48 KiB or contains NUL")]
    InvalidValue,
    #[error("An environment can have at most 100 secrets and 100 variables")]
    Limit,
    #[error("Environment variables exceed 256 KiB in total")]
    Size,
    #[error("Environment secrets exceed 1 MiB in total")]
    SecretsSize,
}

pub fn environment_key(name: &str) -> Result<String, ValueError> {
    if name.trim().is_empty()
        || name.len() > 1024
        || name.chars().count() > 255
        || name.chars().any(char::is_control)
    {
        return Err(ValueError::InvalidEnvironment);
    }
    Ok(name.to_lowercase())
}

pub fn validate(name: &str, value: &str) -> Result<(), ValueError> {
    use codeyard_actions::values::InvalidValue;
    codeyard_actions::values::validate(name, value).map_err(|error| match error {
        InvalidValue::Name | InvalidValue::ReservedName => ValueError::InvalidName,
        InvalidValue::Nul | InvalidValue::TooLarge => ValueError::InvalidValue,
    })
}

native_operation!(crate::backend::db::postgres::environment_values::create,
    crate::backend::db::sqlite::environment_values::create,
    fn create(pool: &DbPool, actor: impl Into<super::credentials::Actor>, repo: i64, name: &str) -> Result<(), ValueError>);
pub const ENVIRONMENTS_PER_PAGE: usize = 50;
native_operation!(crate::backend::db::postgres::environment_values::environments,
    crate::backend::db::sqlite::environment_values::environments,
    fn environments(pool: &DbPool, actor: i64, repo: i64, offset: i64) -> Result<Vec<String>, ValueError>);
native_operation!(crate::backend::db::postgres::environment_values::environment,
    crate::backend::db::sqlite::environment_values::environment,
    fn environment(pool: &DbPool, actor: i64, repo: i64, name: &str) -> Result<Option<String>, ValueError>);
native_operation!(crate::backend::db::postgres::environment_values::list,
    crate::backend::db::sqlite::environment_values::list,
    fn list(pool: &DbPool, actor: i64, repo: i64, environment: &str, kind: Kind) -> Result<Vec<EnvironmentValue>, ValueError>);
native_operation!(crate::backend::db::postgres::environment_values::save,
    crate::backend::db::sqlite::environment_values::save,
    fn save(pool: &DbPool, encryptor: Option<&SecretEncryptor>, actor: impl Into<super::credentials::Actor>, repo: i64, environment: &str, input: &Save<'_>) -> Result<bool, ValueError>);
native_operation!(crate::backend::db::postgres::environment_values::delete,
    crate::backend::db::sqlite::environment_values::delete,
    fn delete(pool: &DbPool, actor: impl Into<super::credentials::Actor>, repo: i64, environment: &str, kind: Kind, name: &str) -> Result<(), ValueError>);

#[derive(sqlx::FromRow)]
pub struct StoredValue {
    pub name: String,
    pub value: String,
    pub encrypted_value: Option<Vec<u8>>,
    pub nonce: Option<Vec<u8>>,
}
native_operation!(crate::backend::db::postgres::environment_values::workflow_values,
    crate::backend::db::sqlite::environment_values::workflow_values,
    fn workflow_values(pool: &DbPool, repo: i64, environment: &str, kind: Kind) -> Result<Vec<StoredValue>, ValueError>);

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WorkflowValues {
    pub secrets: std::collections::BTreeMap<String, String>,
    pub variables: std::collections::BTreeMap<String, String>,
}

pub async fn for_run(
    pool: &DbPool,
    encryptor: Option<&SecretEncryptor>,
    repo: i64,
    environment: &str,
    fork: bool,
) -> Result<WorkflowValues, ValueError> {
    let variables: Vec<_> = workflow_values(pool, repo, environment, Kind::Variable)
        .await?
        .into_iter()
        .map(|row| (row.name, row.value))
        .collect();
    if variables.len() > MAX_SCOPE_VALUES {
        return Err(ValueError::Limit);
    }
    super::variables::validate_all(&variables).map_err(|_| ValueError::Size)?;
    let mut secrets = Vec::new();
    if !fork {
        for row in workflow_values(pool, repo, environment, Kind::Secret).await? {
            let encryptor = encryptor.ok_or(SecretError::InvalidMasterKey)?;
            let value = encryptor.decrypt(
                row.encrypted_value
                    .as_deref()
                    .ok_or(SecretError::DecryptionFailed)?,
                row.nonce.as_deref().ok_or(SecretError::DecryptionFailed)?,
            )?;
            secrets.push((
                row.name,
                String::from_utf8(value).map_err(|_| ValueError::InvalidValue)?,
            ));
        }
    }
    super::secrets::validate_repository_secrets(&secrets).map_err(|_| ValueError::SecretsSize)?;
    Ok(WorkflowValues {
        secrets: secrets.into_iter().collect(),
        variables: variables.into_iter().collect(),
    })
}
