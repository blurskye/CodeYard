use anyhow::Result;
use codeyard_database::native_operation;
use codeyard_database::queries::users::accounts::AccountCredential;
use codeyard_database::DbPool;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct SigningKey {
    pub id: i64,
    pub title: String,
    pub key: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A signing key found by its SHA-256 fingerprint, with its owner.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct SigningKeyOwner {
    pub user_id: i64,
    pub key: String,
}

native_operation!(crate::postgres::users::signing_keys::find_signing_key,
    crate::sqlite::users::signing_keys::find_signing_key,
    fn find_signing_key(pool: &DbPool, fingerprint: &str) -> Result<Option<SigningKeyOwner>>);

native_operation!(crate::postgres::users::signing_keys::list_signing_key_summaries,
    crate::sqlite::users::signing_keys::list_signing_key_summaries,
    /// The account's own settings listing, read while `credential` is still live.
    fn list_signing_key_summaries(pool: &DbPool, user: i64, credential: &AccountCredential<'_>, after: i64) -> Result<Vec<crate::users::ssh_keys::SshKeySummary>>);

native_operation!(crate::postgres::users::signing_keys::list_signing_keys,
    crate::sqlite::users::signing_keys::list_signing_keys,
    fn list_signing_keys(pool: &DbPool, user: i64, limit: i64, offset: i64) -> Result<Vec<SigningKey>>);

native_operation!(crate::postgres::users::signing_keys::get_signing_key,
    crate::sqlite::users::signing_keys::get_signing_key,
    fn get_signing_key(pool: &DbPool, user: i64, id: i64) -> Result<Option<SigningKey>>);

native_operation!(crate::postgres::users::signing_keys::add_signing_key,
    crate::sqlite::users::signing_keys::add_signing_key,
    fn add_signing_key(pool: &DbPool, user: i64, credential: &AccountCredential<'_>, title: &str, key: &str) -> Result<SigningKey>);

native_operation!(crate::postgres::users::signing_keys::delete_signing_key,
    crate::sqlite::users::signing_keys::delete_signing_key,
    fn delete_signing_key(pool: &DbPool, user: i64, credential: &AccountCredential<'_>, id: i64) -> Result<bool>);
