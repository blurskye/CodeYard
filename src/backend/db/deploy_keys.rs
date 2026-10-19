//! Deploy key rows. Callers check who may change them; these operations read and write rows and
//! keep each public key unique across account keys and deploy keys.
use crate::backend::db::models::DeployKey;
use crate::backend::db::DbPool;
use anyhow::Result;

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
    fn list_deploy_keys(pool: &DbPool, repo_id: i64) -> Result<Vec<DeployKey>>);

native_operation!(crate::backend::db::postgres::deploy_keys::get_deploy_key,
    crate::backend::db::sqlite::deploy_keys::get_deploy_key,
    fn get_deploy_key(pool: &DbPool, key_id: i64) -> Result<Option<DeployKey>>);

native_operation!(crate::backend::db::postgres::deploy_keys::get_deploy_key_by_fingerprint,
    crate::backend::db::sqlite::deploy_keys::get_deploy_key_by_fingerprint,
    fn get_deploy_key_by_fingerprint(pool: &DbPool, fingerprint: &str) -> Result<Option<DeployKey>>);

// Refused with `SshKeyWriteRefusal::AlreadyUsed` when the key is an account key or a deploy key.
native_operation!(crate::backend::db::postgres::deploy_keys::insert_deploy_key,
    crate::backend::db::sqlite::deploy_keys::insert_deploy_key,
    fn insert_deploy_key(pool: &DbPool, key: &NewDeployKey) -> Result<DeployKey>);

// Deletes only a key of `repo_id`. Returns false when no such key exists.
native_operation!(crate::backend::db::postgres::deploy_keys::delete_deploy_key,
    crate::backend::db::sqlite::deploy_keys::delete_deploy_key,
    fn delete_deploy_key(pool: &DbPool, repo_id: i64, key_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::deploy_keys::touch_deploy_key,
    crate::backend::db::sqlite::deploy_keys::touch_deploy_key,
    fn touch_deploy_key(pool: &DbPool, key_id: i64) -> Result<()>);
