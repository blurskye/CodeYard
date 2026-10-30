use super::accounts::AccountCredential;
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct GpgMaterial {
    pub public_key: String,
    pub raw_key: Option<String>,
    pub emails: Vec<String>,
    pub can_sign: bool,
    pub can_encrypt_comms: bool,
    pub can_encrypt_storage: bool,
    pub can_certify: bool,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked: bool,
}
#[derive(Clone, Debug)]
pub struct NewGpgKey {
    pub fingerprint: String,
    pub key_id: String,
    pub material: GpgMaterial,
    pub subkeys: Vec<NewGpgKey>,
}
#[derive(Clone, Debug)]
pub struct GpgKey {
    pub id: i64,
    pub user_id: i64,
    pub primary_key_id: Option<i64>,
    pub name: Option<String>,
    pub fingerprint: String,
    pub key_id: String,
    pub material: GpgMaterial,
}
#[derive(sqlx::FromRow)]
pub(crate) struct StoredGpgKey {
    pub id: i64,
    pub user_id: i64,
    pub primary_key_id: Option<i64>,
    pub name: Option<String>,
    pub fingerprint: String,
    pub key_id: String,
    pub key_data: String,
}
impl StoredGpgKey {
    pub(crate) fn decode(self) -> Result<GpgKey> {
        Ok(GpgKey {
            id: self.id,
            user_id: self.user_id,
            primary_key_id: self.primary_key_id,
            name: self.name,
            fingerprint: self.fingerprint,
            key_id: self.key_id,
            material: serde_json::from_str(&self.key_data)?,
        })
    }
}
/// What the settings page shows of a primary key, read without its certificate.
#[derive(Clone, Debug)]
pub struct GpgKeySummary {
    pub id: i64,
    pub name: Option<String>,
    pub fingerprint: String,
    pub key_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked: bool,
}
/// The summary columns as both engines return them: the stored JSON's timestamp text.
#[derive(sqlx::FromRow)]
pub(crate) struct StoredGpgKeySummary {
    pub id: i64,
    pub name: Option<String>,
    pub fingerprint: String,
    pub key_id: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub revoked: bool,
}
impl StoredGpgKeySummary {
    pub(crate) fn decode(self) -> Result<GpgKeySummary> {
        Ok(GpgKeySummary {
            id: self.id,
            name: self.name,
            fingerprint: self.fingerprint,
            key_id: self.key_id,
            created_at: self.created_at.parse::<DateTime<Utc>>()?,
            expires_at: self.expires_at.map(|value| value.parse::<DateTime<Utc>>()).transpose()?,
            revoked: self.revoked,
        })
    }
}
#[derive(Debug, thiserror::Error)]
pub enum GpgKeyRefusal {
    #[error("This GPG key is already registered")]
    AlreadyUsed,
    #[error("At most 100 GPG keys may be registered")]
    Limit,
}

native_operation!(crate::backend::db::postgres::users::gpg_keys::list_gpg_keys,
    crate::backend::db::sqlite::users::gpg_keys::list_gpg_keys,
    fn list_gpg_keys(pool: &DbPool, user: i64, limit: i64, offset: i64) -> Result<Vec<GpgKey>>);
native_operation!(crate::backend::db::postgres::users::gpg_keys::list_gpg_key_summaries,
    crate::backend::db::sqlite::users::gpg_keys::list_gpg_key_summaries,
    /// The account's own settings listing of its first 100 primary keys, read while
    /// `credential` is still live.
    fn list_gpg_key_summaries(pool: &DbPool, user: i64, credential: &AccountCredential<'_>) -> Result<Vec<GpgKeySummary>>);
native_operation!(crate::backend::db::postgres::users::gpg_keys::get_gpg_key,
    crate::backend::db::sqlite::users::gpg_keys::get_gpg_key,
    fn get_gpg_key(pool: &DbPool, user: i64, id: i64) -> Result<Option<GpgKey>>);
native_operation!(crate::backend::db::postgres::users::gpg_keys::list_subkeys,
    crate::backend::db::sqlite::users::gpg_keys::list_subkeys,
    fn list_subkeys(pool: &DbPool, user: i64, parents: &[i64]) -> Result<Vec<GpgKey>>);
native_operation!(crate::backend::db::postgres::users::gpg_keys::find_signing_keys,
    crate::backend::db::sqlite::users::gpg_keys::find_signing_keys,
    fn find_signing_keys(pool: &DbPool, issuer: &str) -> Result<Vec<GpgKey>>);
native_operation!(crate::backend::db::postgres::users::gpg_keys::add_gpg_key,
    crate::backend::db::sqlite::users::gpg_keys::add_gpg_key,
    fn add_gpg_key(pool: &DbPool, user: i64, credential: &AccountCredential<'_>, name: Option<&str>, key: &NewGpgKey) -> Result<GpgKey>);
native_operation!(crate::backend::db::postgres::users::gpg_keys::delete_gpg_key,
    crate::backend::db::sqlite::users::gpg_keys::delete_gpg_key,
    fn delete_gpg_key(pool: &DbPool, user: i64, credential: &AccountCredential<'_>, id: i64) -> Result<bool>);
