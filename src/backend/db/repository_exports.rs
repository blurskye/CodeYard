//! User and organization exports retain their scope after the creating token expires.
use super::DbPool;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ExportOptions {
    #[serde(default)]
    pub lock_repositories: bool,
    #[serde(default)]
    pub exclude_metadata: bool,
    #[serde(default)]
    pub exclude_git_data: bool,
    #[serde(default)]
    pub exclude_attachments: bool,
    #[serde(default)]
    pub exclude_releases: bool,
    #[serde(default)]
    pub exclude_owner_projects: bool,
    #[serde(default)]
    pub org_metadata_only: bool,
}

#[derive(Clone, sqlx::FromRow)]
pub struct RepositoryExport {
    pub id: i64,
    pub guid: String,
    pub owner_user_id: Option<i64>,
    pub owner_org_id: Option<i64>,
    pub actor_id: Option<i64>,
    pub repositories: String,
    pub options: String,
    pub state: String,
    pub archive_deleted: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl RepositoryExport {
    pub fn object_key(&self) -> String {
        format!("migrations/{}", self.guid)
    }
    pub fn repository_ids(&self) -> Result<Vec<i64>> {
        Ok(serde_json::from_str(&self.repositories)?)
    }
    pub fn options(&self) -> Result<ExportOptions> {
        Ok(serde_json::from_str(&self.options)?)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExportRefusal {
    #[error("Migration ownership denied")]
    Denied,
    #[error("Invalid repository selection")]
    Invalid,
    #[error("Repository is already locked for migration")]
    Locked,
    #[error("Too many active migrations")]
    Busy,
    #[error("Migration archive is not available")]
    Archive,
}

native_operation!(crate::backend::db::postgres::repository_exports::create,
    crate::backend::db::sqlite::repository_exports::create,
    fn create(pool: &DbPool, actor: i64, org: Option<i64>, ids: &[i64], options: &ExportOptions) -> Result<RepositoryExport>);
native_operation!(crate::backend::db::postgres::repository_exports::get,
    crate::backend::db::sqlite::repository_exports::get,
    fn get(pool: &DbPool, actor: i64, org: Option<i64>, id: i64) -> Result<Option<RepositoryExport>>);
native_operation!(crate::backend::db::postgres::repository_exports::by_guid,
    crate::backend::db::sqlite::repository_exports::by_guid,
    fn by_guid(pool: &DbPool, actor: i64, guid: &str) -> Result<Option<RepositoryExport>>);
native_operation!(crate::backend::db::postgres::repository_exports::list,
    crate::backend::db::sqlite::repository_exports::list,
    fn list(pool: &DbPool, actor: i64, org: Option<i64>, limit: i64, offset: i64) -> Result<Vec<RepositoryExport>>);
native_operation!(crate::backend::db::postgres::repository_exports::finish,
    crate::backend::db::sqlite::repository_exports::finish,
    fn finish(pool: &DbPool, id: i64, state: &str) -> Result<()>);
native_operation!(crate::backend::db::postgres::repository_exports::delete_archive,
    crate::backend::db::sqlite::repository_exports::delete_archive,
    fn delete_archive(pool: &DbPool, actor: i64, org: Option<i64>, id: i64) -> Result<Option<RepositoryExport>>);
native_operation!(crate::backend::db::postgres::repository_exports::lock_repository,
    crate::backend::db::sqlite::repository_exports::lock_repository,
    fn lock_repository(pool: &DbPool, actor: i64, migration: i64, repo: i64) -> Result<()>);
native_operation!(crate::backend::db::postgres::repository_exports::unlock_repository,
    crate::backend::db::sqlite::repository_exports::unlock_repository,
    fn unlock_repository(pool: &DbPool, actor: i64, org: Option<i64>, migration: i64, name: &str) -> Result<bool>);
native_operation!(crate::backend::db::postgres::repository_exports::locked,
    crate::backend::db::sqlite::repository_exports::locked,
    fn locked(pool: &DbPool, repo: i64) -> Result<bool>);
