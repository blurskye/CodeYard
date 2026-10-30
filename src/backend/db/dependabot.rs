//! Organization Dependabot credentials are separate from workflow secrets.
use super::{models::Repository, secrets::SecretsError, DbPool};
use crate::backend::crypto::SecretEncryptor;
use chrono::{DateTime, Utc};

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
        super::secrets::validate_repository_secret(self.name, self.value.unwrap_or(""))?;
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

native_operation!(super::postgres::dependabot::repository_access,
    super::sqlite::dependabot::repository_access,
    fn repository_access(pool: &DbPool, actor: impl Into<super::credentials::Actor>, org: i64)
    -> Result<(String, Vec<Repository>), SecretsError>);

native_operation!(super::postgres::dependabot::update_repository_access,
    super::sqlite::dependabot::update_repository_access,
    fn update_repository_access(pool: &DbPool, actor: impl Into<super::credentials::Actor>, org: i64, add: &[i64], remove: &[i64])
    -> Result<(), SecretsError>);

native_operation!(super::postgres::dependabot::set_repository_access_default,
    super::sqlite::dependabot::set_repository_access_default,
    fn set_repository_access_default(pool: &DbPool, actor: impl Into<super::credentials::Actor>, org: i64, level: &str)
    -> Result<(), SecretsError>);

pub enum RepositorySelection<'a> {
    Set(&'a [i64]),
    Add(i64),
    Remove(i64),
}

native_operation!(super::postgres::dependabot::organization_key,
    super::sqlite::dependabot::organization_key,
    fn organization_key(pool: &DbPool, encryptor: &SecretEncryptor, actor: impl Into<super::credentials::Actor>, org: i64)
    -> Result<(String, crypto_box::SecretKey), SecretsError>);

native_operation!(super::postgres::dependabot::list_organization_secrets,
    super::sqlite::dependabot::list_organization_secrets,
    fn list_organization_secrets(pool: &DbPool, actor: impl Into<super::credentials::Actor>, org: i64)
    -> Result<Vec<OrganizationSecret>, SecretsError>);

native_operation!(super::postgres::dependabot::save_organization_secret,
    super::sqlite::dependabot::save_organization_secret,
    fn save_organization_secret(pool: &DbPool, encryptor: &SecretEncryptor, actor: impl Into<super::credentials::Actor>, org: i64, input: &OrganizationSecretInput<'_>)
    -> Result<bool, SecretsError>);

native_operation!(super::postgres::dependabot::delete_organization_secret,
    super::sqlite::dependabot::delete_organization_secret,
    fn delete_organization_secret(pool: &DbPool, actor: impl Into<super::credentials::Actor>, org: i64, name: &str)
    -> Result<bool, SecretsError>);

native_operation!(super::postgres::dependabot::selected_repositories,
    super::sqlite::dependabot::selected_repositories,
    fn selected_repositories(pool: &DbPool, actor: impl Into<super::credentials::Actor>, org: i64, name: &str)
    -> Result<Vec<Repository>, SecretsError>);

native_operation!(super::postgres::dependabot::select_repositories,
    super::sqlite::dependabot::select_repositories,
    fn select_repositories(pool: &DbPool, actor: impl Into<super::credentials::Actor>, org: i64, name: &str, selection: RepositorySelection<'_>)
    -> Result<(), SecretsError>);
