//! Organization Actions values and their repository visibility.
use super::{users::accounts::AccountCredential, DbPool};
use crate::backend::crypto::{SecretEncryptor, SecretError};
use chrono::{DateTime, Utc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Secret,
    Variable,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Secret => "secret",
            Self::Variable => "variable",
        }
    }
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct OrganizationValue {
    pub name: String,
    pub kind: String,
    pub value: String,
    pub visibility: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const SETTINGS_PAGE_SIZE: i64 = 30;

#[derive(sqlx::FromRow)]
pub struct SettingsValue {
    #[sqlx(flatten)]
    pub value: OrganizationValue,
    pub repository_ids: String,
}

pub struct SettingsPage {
    pub values: Vec<SettingsValue>,
    pub more: bool,
}
impl SettingsPage {
    pub(crate) fn from_rows(mut values: Vec<SettingsValue>) -> Self {
        let more = values.len() > SETTINGS_PAGE_SIZE as usize;
        values.truncate(SETTINGS_PAGE_SIZE as usize);
        Self { values, more }
    }
}

pub struct Save<'a> {
    pub kind: Kind,
    pub previous: Option<&'a str>,
    pub name: &'a str,
    pub value: Option<&'a str>,
    pub visibility: Option<&'a str>,
    pub repositories: Option<&'a [i64]>,
}

pub enum Selection<'a> {
    Set(&'a [i64]),
    Add(i64),
    Remove(i64),
}

#[derive(Debug, thiserror::Error)]
pub enum ValueError {
    #[error("Actions value database unavailable")]
    Database(#[from] sqlx::Error),
    #[error("Actions value authority unavailable")]
    Authority(#[source] anyhow::Error),
    #[error("Actions secret encryption unavailable")]
    Encryption(#[from] SecretError),
    #[error("Actions value access denied")]
    Denied,
    #[error("Actions value not found")]
    NotFound,
    #[error("Actions value already exists")]
    Exists,
    #[error("The value must have selected repository visibility")]
    NotSelected,
    #[error("Actions value name is invalid or reserved")]
    InvalidName,
    #[error("Actions value exceeds 48 KiB or contains NUL")]
    InvalidValue,
    #[error("Choose all, private or selected repository visibility")]
    Visibility,
    #[error("Selected repositories must belong to this organization")]
    Repository,
    #[error("An organization can have at most 1000 values of each kind")]
    Limit,
    #[error("Delivered Actions secrets exceed 1 MiB in total")]
    SecretsSize,
}

/// Whether organization value `v` reaches repository `r`. Listing the inherited values and
/// delivering them to a workflow both use it, so what settings show is what a run receives.
pub(crate) const REACHES_REPOSITORY: &str = "(v.visibility='all' OR (v.visibility='private' AND r.is_private) OR (v.visibility='selected' AND EXISTS(SELECT 1 FROM org_actions_value_repositories s WHERE s.org_id=v.org_id AND s.kind=v.kind AND s.name=v.name AND s.repo_id=r.id)))";

pub fn validate(name: &str, value: &str) -> Result<(), ValueError> {
    use codeyard_actions::values::InvalidValue;
    codeyard_actions::values::validate(name, value).map_err(|error| match error {
        InvalidValue::Name | InvalidValue::ReservedName => ValueError::InvalidName,
        InvalidValue::Nul | InvalidValue::TooLarge => ValueError::InvalidValue,
    })
}

native_operation!(crate::backend::db::postgres::organizations::action_values::key,
    crate::backend::db::sqlite::organizations::action_values::key,
    fn key(pool: &DbPool, encryptor: &SecretEncryptor, actor: i64, org: i64) -> Result<(String, crypto_box::SecretKey), ValueError>);
native_operation!(crate::backend::db::postgres::organizations::action_values::list,
    crate::backend::db::sqlite::organizations::action_values::list,
    fn list(pool: &DbPool, actor: i64, org: i64, kind: Kind) -> Result<Vec<OrganizationValue>, ValueError>);
native_operation!(crate::backend::db::postgres::organizations::action_values::settings_page,
    crate::backend::db::sqlite::organizations::action_values::settings_page,
    fn settings_page(pool: &DbPool, actor: i64, session: &str, org: i64, page: i64) -> Result<SettingsPage, ValueError>);
native_operation!(crate::backend::db::postgres::organizations::action_values::save,
    crate::backend::db::sqlite::organizations::action_values::save,
    fn save(pool: &DbPool, encryptor: Option<&SecretEncryptor>, actor: i64, credential: &AccountCredential<'_>, org: i64, input: &Save<'_>) -> Result<bool, ValueError>);
native_operation!(crate::backend::db::postgres::organizations::action_values::delete,
    crate::backend::db::sqlite::organizations::action_values::delete,
    fn delete(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, org: i64, kind: Kind, name: &str) -> Result<(), ValueError>);
native_operation!(crate::backend::db::postgres::organizations::action_values::repositories,
    crate::backend::db::sqlite::organizations::action_values::repositories,
    fn repositories(pool: &DbPool, actor: i64, org: i64, kind: Kind, name: &str, offset: i64, limit: i64) -> Result<(i64, Vec<super::repository_stats::RepositorySnapshot>), ValueError>);
native_operation!(crate::backend::db::postgres::organizations::action_values::select,
    crate::backend::db::sqlite::organizations::action_values::select,
    fn select(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, org: i64, kind: Kind, name: &str, change: Selection<'_>) -> Result<(), ValueError>);
native_operation!(crate::backend::db::postgres::organizations::action_values::inherited,
    crate::backend::db::sqlite::organizations::action_values::inherited,
    fn inherited(pool: &DbPool, actor: i64, repo: i64, kind: Kind) -> Result<Vec<OrganizationValue>, ValueError>);

#[derive(sqlx::FromRow)]
pub struct StoredValue {
    pub name: String,
    pub value: String,
    pub visibility: String,
    pub encrypted_value: Option<Vec<u8>>,
    pub nonce: Option<Vec<u8>>,
}
native_operation!(crate::backend::db::postgres::organizations::action_values::workflow_values,
    crate::backend::db::sqlite::organizations::action_values::workflow_values,
    fn workflow_values(pool: &DbPool, repo: i64, kind: Kind) -> Result<Vec<StoredValue>, ValueError>);

pub async fn secrets_for_run(
    pool: &DbPool,
    encryptor: &SecretEncryptor,
    repo: i64,
) -> Result<Vec<(String, String)>, ValueError> {
    let mut result = Vec::new();
    for row in workflow_values(pool, repo, Kind::Secret).await? {
        let value = encryptor.decrypt(
            row.encrypted_value
                .as_deref()
                .ok_or(SecretError::DecryptionFailed)?,
            row.nonce.as_deref().ok_or(SecretError::DecryptionFailed)?,
        )?;
        result.push((
            row.name,
            String::from_utf8(value).map_err(|_| ValueError::InvalidValue)?,
        ));
    }
    Ok(result)
}

pub async fn variables_for_run(
    pool: &DbPool,
    repo: i64,
) -> Result<Vec<(String, String)>, ValueError> {
    Ok(workflow_values(pool, repo, Kind::Variable)
        .await?
        .into_iter()
        .map(|row| (row.name, row.value))
        .collect())
}
