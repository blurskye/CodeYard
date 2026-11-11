use anyhow::Result;
use codeyard_database::models::SshKey;
use codeyard_database::models::UserWithKey;
use codeyard_database::native_operation;
use codeyard_database::DbPool;
use codeyard_security::ssh_keys::parse_public_key;
use ssh_key::HashAlg;

#[derive(Debug, sqlx::FromRow)]
pub struct SshKeySummary {
    pub id: i64,
    pub name: String,
    pub fingerprint: String,
    pub key_type: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

native_operation!(crate::postgres::users::ssh_keys::list_user_key_summaries,
    crate::sqlite::users::ssh_keys::list_user_key_summaries,
    /// The account's own settings listing, read while `credential` is still live.
    fn list_user_key_summaries(pool: &DbPool, user_id: i64, credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>, after: i64) -> Result<Vec<SshKeySummary>>);

pub(crate) fn valid_stored_key(key: &SshKey) -> bool {
    parse_public_key(&key.public_key).is_ok_and(|parsed| {
        parsed.algorithm().to_string() == key.key_type
            && parsed.fingerprint(HashAlg::Sha256).to_string() == key.fingerprint
    })
}

native_operation!(crate::postgres::users::ssh_keys::add_ssh_key,
    crate::sqlite::users::ssh_keys::add_ssh_key,
    fn add_ssh_key(
    pool: &DbPool,
    user_id: i64,
    name: &str,
    public_key: &str,
) -> Result<SshKey>);

native_operation!(crate::postgres::users::ssh_keys::add_ssh_key_for_token,
    crate::sqlite::users::ssh_keys::add_ssh_key_for_token,
    fn add_ssh_key_for_token(pool: &DbPool, user_id: i64, token: i64, hash: &str, name: &str, public_key: &str) -> Result<SshKey>);

native_operation!(crate::postgres::users::ssh_keys::add_ssh_key_for_session,
    crate::sqlite::users::ssh_keys::add_ssh_key_for_session,
    fn add_ssh_key_for_session(
    pool: &DbPool,
    user_id: i64,
    session_id: &str,
    name: &str,
    public_key: &str,
) -> Result<SshKey>);

native_operation!(crate::postgres::users::ssh_keys::get_user_by_key_fingerprint,
    crate::sqlite::users::ssh_keys::get_user_by_key_fingerprint,
    fn get_user_by_key_fingerprint(
    pool: &DbPool,
    fingerprint: &str,
) -> Result<Option<UserWithKey>>);

native_operation!(crate::postgres::users::ssh_keys::touch_ssh_key,
    crate::sqlite::users::ssh_keys::touch_ssh_key,
    fn touch_ssh_key(pool: &DbPool, key_id: i64) -> Result<()>);

native_operation!(crate::postgres::users::ssh_keys::get_user_by_ssh_key_id,
    crate::sqlite::users::ssh_keys::get_user_by_ssh_key_id,
    fn get_user_by_ssh_key_id(pool: &DbPool, key_id: i64) -> Result<Option<UserWithKey>>);

native_operation!(crate::postgres::users::ssh_keys::list_user_keys,
    crate::sqlite::users::ssh_keys::list_user_keys,
    fn list_user_keys(pool: &DbPool, user_id: i64) -> Result<Vec<SshKey>>);

// One page of an account's keys, oldest first.
native_operation!(crate::postgres::users::ssh_keys::user_keys_page,
    crate::sqlite::users::ssh_keys::user_keys_page,
    fn user_keys_page(pool: &DbPool, user_id: i64, offset: i64, limit: i64) -> Result<Vec<SshKey>>);

native_operation!(crate::postgres::users::ssh_keys::get_user_key,
    crate::sqlite::users::ssh_keys::get_user_key,
    fn get_user_key(pool: &DbPool, user_id: i64, key_id: i64) -> Result<Option<SshKey>>);

native_operation!(crate::postgres::users::ssh_keys::delete_user_ssh_key_for_token,
    crate::sqlite::users::ssh_keys::delete_user_ssh_key_for_token,
    fn delete_user_ssh_key_for_token(pool: &DbPool, user_id: i64, token: i64, hash: &str, key_id: i64) -> Result<bool>);

native_operation!(crate::postgres::users::ssh_keys::delete_user_ssh_key,
    crate::sqlite::users::ssh_keys::delete_user_ssh_key,
    fn delete_user_ssh_key(pool: &DbPool, user_id: i64, credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>, key_id: i64) -> Result<bool>);

native_operation!(crate::postgres::users::ssh_keys::delete_ssh_key,
    crate::sqlite::users::ssh_keys::delete_ssh_key,
    fn delete_ssh_key(pool: &DbPool, key_id: i64) -> Result<bool>);
