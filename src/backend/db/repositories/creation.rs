//! Creating repositories.
use crate::backend::db::models::Repository;
use crate::backend::db::DbPool;
use anyhow::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepositoryTarget {
    Personal(i64),
    Organization(i64),
}

/// Who is creating a repository, and for which owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepositoryCreationOwner {
    pub actor_id: i64,
    pub target: RepositoryTarget,
}
impl RepositoryCreationOwner {
    pub fn personal(actor_id: i64) -> Self {
        Self {
            actor_id,
            target: RepositoryTarget::Personal(actor_id),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RepositoryStorageRefusal {
    #[error("Repository creation authority is denied")]
    Denied,
    #[error("Fork source storage placement changed")]
    StalePlacement,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CreationAdmission<'a> {
    pub authority: Option<&'a crate::backend::db::credentials::Actor>,
    /// Only draft advisory remediation may have multiple forks per personal owner.
    pub advisory: Option<&'a str>,
}

pub struct NewRepository<'a> {
    pub owner: RepositoryCreationOwner,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub is_private: bool,
    pub disk_path: &'a str,
    pub storage_node_id: Option<i64>,
    pub source_id: Option<i64>,
}

native_operation!(crate::backend::db::postgres::repositories::creation::check_creation_authority,
    crate::backend::db::sqlite::repositories::creation::check_creation_authority,
    fn check_creation_authority(pool: &DbPool, owner: RepositoryCreationOwner) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::creation::insert_repository,
    crate::backend::db::sqlite::repositories::creation::insert_repository,
    fn insert_repository(pool: &DbPool, input: NewRepository<'_>) -> Result<Option<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::creation::insert_repository_with_properties,
    crate::backend::db::sqlite::repositories::creation::insert_repository_with_properties,
    fn insert_repository_with_properties(pool: &DbPool, input: NewRepository<'_>, properties: &[crate::backend::db::organizations::properties::ValueInput]) -> Result<Option<Repository>>);
native_operation!(crate::backend::db::postgres::repositories::creation::insert_repository_with_admission,
    crate::backend::db::sqlite::repositories::creation::insert_repository_with_admission,
    fn insert_repository_with_admission(pool: &DbPool, input: NewRepository<'_>, properties: &[crate::backend::db::organizations::properties::ValueInput], admission: CreationAdmission<'_>) -> Result<Option<Repository>>);
native_operation!(crate::backend::db::postgres::repositories::creation::insert_gist_repository,crate::backend::db::sqlite::repositories::creation::insert_gist_repository,
    fn insert_gist_repository(pool: &DbPool,input: NewRepository<'_>) -> Result<Option<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::creation::repository_visibility_locked,
    crate::backend::db::sqlite::repositories::creation::repository_visibility_locked,
    fn repository_visibility_locked(pool: &DbPool, repo_id: i64) -> Result<bool>);
