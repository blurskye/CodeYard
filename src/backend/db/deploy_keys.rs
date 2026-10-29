//! Deploy keys: mutations retain live session and repository-admin authority and keep
//! each public key unique across account keys and deploy keys.
use crate::backend::db::models::DeployKey;
use crate::backend::db::DbPool;
use anyhow::Result;

/// A live browser session or write token, checked with the repository mutation.
pub enum Credential {
    Session(String),
    Token(i64),
}

impl From<&str> for Credential {
    fn from(session: &str) -> Self {
        Self::Session(session.to_owned())
    }
}
impl From<&String> for Credential {
    fn from(session: &String) -> Self {
        Self::Session(session.clone())
    }
}

/// A deploy key to store. The fingerprint and key type come from the validated public key.
#[derive(Debug, Clone)]
pub struct NewDeployKey {
    pub repo_id: i64,
    pub title: String,
    pub fingerprint: String,
    pub public_key: String,
    pub can_write: bool,
    pub created_by_id: i64,
}

native_operation!(crate::backend::db::postgres::deploy_keys::list_deploy_keys,
    crate::backend::db::sqlite::deploy_keys::list_deploy_keys,
    fn list_deploy_keys(pool: &DbPool, repo_id: i64, after: i64, limit: i64) -> Result<Vec<DeployKey>>);

native_operation!(crate::backend::db::postgres::deploy_keys::get_deploy_key,
    crate::backend::db::sqlite::deploy_keys::get_deploy_key,
    fn get_deploy_key(pool: &DbPool, key_id: i64) -> Result<Option<DeployKey>>);

// Git credential lookup checks the repository's current organization policy, not a cached owner.
// A fingerprint that is also an account key is ambiguous and authenticates as neither.
native_operation!(crate::backend::db::postgres::deploy_keys::usable_deploy_key,
    crate::backend::db::sqlite::deploy_keys::usable_deploy_key,
    fn usable_deploy_key(pool:&DbPool,key_id:i64)->Result<Option<DeployKey>>);

native_operation!(crate::backend::db::postgres::deploy_keys::get_deploy_key_by_fingerprint,
    crate::backend::db::sqlite::deploy_keys::get_deploy_key_by_fingerprint,
    fn get_deploy_key_by_fingerprint(pool: &DbPool, fingerprint: &str) -> Result<Option<DeployKey>>);

// Refused with `SshKeyWriteRefusal::AlreadyUsed` when the key is an account key or a deploy key.
native_operation!(crate::backend::db::postgres::deploy_keys::insert_deploy_key,
    crate::backend::db::sqlite::deploy_keys::insert_deploy_key,
    fn insert_deploy_key(pool: &DbPool, key: &NewDeployKey, credential: impl Into<Credential>) -> Result<DeployKey>);

// Deletes only a key of `repo_id`. Returns false when no such key exists.
native_operation!(crate::backend::db::postgres::deploy_keys::delete_deploy_key,
    crate::backend::db::sqlite::deploy_keys::delete_deploy_key,
    fn delete_deploy_key(pool: &DbPool, repo_id: i64, key_id: i64, actor: i64, credential: impl Into<Credential>) -> Result<bool>);

native_operation!(crate::backend::db::postgres::deploy_keys::touch_deploy_key,
    crate::backend::db::sqlite::deploy_keys::touch_deploy_key,
    fn touch_deploy_key(pool: &DbPool, key_id: i64) -> Result<()>);
