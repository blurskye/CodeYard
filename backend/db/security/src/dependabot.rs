use chrono::DateTime;
use chrono::Utc;
use codeyard_database::models::Repository;
use codeyard_database::native_operation;
use codeyard_database::queries::secrets::SecretsError;
use codeyard_database::DbPool;
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
    pub fn validate(&self) -> Result<(), SecretsError> {
        codeyard_db_actions::secrets::validate_repository_secret(
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

// `window` is (offset, limit); callers ask for one extra row to learn whether a next page exists.
native_operation!(crate::postgres::dependabot::repository_access,
    crate::sqlite::dependabot::repository_access,
    fn repository_access(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, window: (i64, i64))
    -> Result<(String, Vec<Repository>), SecretsError>);

native_operation!(crate::postgres::dependabot::update_repository_access,
    crate::sqlite::dependabot::update_repository_access,
    fn update_repository_access(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, add: &[i64], remove: &[i64])
    -> Result<(), SecretsError>);

native_operation!(crate::postgres::dependabot::set_repository_access_default,
    crate::sqlite::dependabot::set_repository_access_default,
    fn set_repository_access_default(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, level: &str)
    -> Result<(), SecretsError>);

pub enum RepositorySelection<'a> {
    Set(&'a [i64]),
    Add(i64),
    Remove(i64),
}

native_operation!(crate::postgres::dependabot::organization_key,
    crate::sqlite::dependabot::organization_key,
    fn organization_key(pool: &DbPool, encryptor: &SecretEncryptor, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64)
    -> Result<(String, crypto_box::SecretKey), SecretsError>);

native_operation!(crate::postgres::dependabot::list_organization_secrets,
    crate::sqlite::dependabot::list_organization_secrets,
    fn list_organization_secrets(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64)
    -> Result<Vec<OrganizationSecret>, SecretsError>);

native_operation!(crate::postgres::dependabot::save_organization_secret,
    crate::sqlite::dependabot::save_organization_secret,
    fn save_organization_secret(pool: &DbPool, encryptor: &SecretEncryptor, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, input: &OrganizationSecretInput<'_>)
    -> Result<bool, SecretsError>);

native_operation!(crate::postgres::dependabot::delete_organization_secret,
    crate::sqlite::dependabot::delete_organization_secret,
    fn delete_organization_secret(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, name: &str)
    -> Result<bool, SecretsError>);

native_operation!(crate::postgres::dependabot::selected_repositories,
    crate::sqlite::dependabot::selected_repositories,
    fn selected_repositories(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, name: &str)
    -> Result<Vec<Repository>, SecretsError>);

native_operation!(crate::postgres::dependabot::select_repositories,
    crate::sqlite::dependabot::select_repositories,
    fn select_repositories(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, name: &str, selection: RepositorySelection<'_>)
    -> Result<(), SecretsError>);
