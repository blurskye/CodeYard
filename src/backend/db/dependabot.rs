//! Organization Dependabot credentials are separate from workflow secrets.
use crate::backend::db::secrets::SecretsError;
use chrono::{DateTime, Utc};
use codeyard_db_core::models::Repository;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use codeyard_support::secrets::SecretEncryptor;

#[derive(sqlx::FromRow, serde::Serialize)]
pub struct OrganizationSecret {
    pub name: String,
    pub visibility: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct OrganizationSecretInput<'a> {
    pub name: &'a str,
    pub value: Option<&'a str>,
    pub visibility: &'a str,
    pub repositories: Option<&'a [i64]>,
}

impl OrganizationSecretInput<'_> {
    pub(crate) fn validate(&self) -> Result<(), SecretsError> {
        crate::backend::db::secrets::validate_repository_secret(
            self.name,
            self.value.unwrap_or(""),
        )?;
        if !matches!(self.visibility, "all" | "private" | "selected")
            || (self.repositories.is_some() && self.visibility != "selected")
            || self
                .repositories
                .is_some_and(|ids| ids.len() > 1000 || ids.iter().any(|id| *id <= 0))
        {
            return Err(SecretsError::InvalidRepositorySelection);
        }
        Ok(())
    }
}

pub(crate) fn validate_repository_access(add: &[i64], remove: &[i64]) -> Result<(), SecretsError> {
    if add.len() > 1000
        || remove.len() > 1000
        || add.iter().chain(remove).any(|id| *id <= 0)
        || add.iter().any(|id| remove.contains(id))
    {
        return Err(SecretsError::InvalidRepositorySelection);
    }
    Ok(())
}

// `window` is (offset, limit); callers ask for one extra row to learn whether a next page exists.
native_operation!(crate::backend::db::postgres::dependabot::repository_access,
    crate::backend::db::sqlite::dependabot::repository_access,
    fn repository_access(pool: &DbPool, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64, window: (i64, i64))
    -> Result<(String, Vec<Repository>), SecretsError>);

native_operation!(crate::backend::db::postgres::dependabot::update_repository_access,
    crate::backend::db::sqlite::dependabot::update_repository_access,
    fn update_repository_access(pool: &DbPool, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64, add: &[i64], remove: &[i64])
    -> Result<(), SecretsError>);

native_operation!(crate::backend::db::postgres::dependabot::set_repository_access_default,
    crate::backend::db::sqlite::dependabot::set_repository_access_default,
    fn set_repository_access_default(pool: &DbPool, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64, level: &str)
    -> Result<(), SecretsError>);

pub enum RepositorySelection<'a> {
    Set(&'a [i64]),
    Add(i64),
    Remove(i64),
}

native_operation!(crate::backend::db::postgres::dependabot::organization_key,
    crate::backend::db::sqlite::dependabot::organization_key,
    fn organization_key(pool: &DbPool, encryptor: &SecretEncryptor, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64)
    -> Result<(String, crypto_box::SecretKey), SecretsError>);

native_operation!(crate::backend::db::postgres::dependabot::list_organization_secrets,
    crate::backend::db::sqlite::dependabot::list_organization_secrets,
    fn list_organization_secrets(pool: &DbPool, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64)
    -> Result<Vec<OrganizationSecret>, SecretsError>);

native_operation!(crate::backend::db::postgres::dependabot::save_organization_secret,
    crate::backend::db::sqlite::dependabot::save_organization_secret,
    fn save_organization_secret(pool: &DbPool, encryptor: &SecretEncryptor, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64, input: &OrganizationSecretInput<'_>)
    -> Result<bool, SecretsError>);

native_operation!(crate::backend::db::postgres::dependabot::delete_organization_secret,
    crate::backend::db::sqlite::dependabot::delete_organization_secret,
    fn delete_organization_secret(pool: &DbPool, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64, name: &str)
    -> Result<bool, SecretsError>);

native_operation!(crate::backend::db::postgres::dependabot::selected_repositories,
    crate::backend::db::sqlite::dependabot::selected_repositories,
    fn selected_repositories(pool: &DbPool, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64, name: &str)
    -> Result<Vec<Repository>, SecretsError>);

native_operation!(crate::backend::db::postgres::dependabot::select_repositories,
    crate::backend::db::sqlite::dependabot::select_repositories,
    fn select_repositories(pool: &DbPool, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64, name: &str, selection: RepositorySelection<'_>)
    -> Result<(), SecretsError>);
