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
    #[error("Organization forks are not supported")]
    UnsupportedOrganizationFork,
    #[error("Fork source storage placement changed")]
    StalePlacement,
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

native_operation!(crate::backend::db::postgres::repositories::creation::insert_repository,
    crate::backend::db::sqlite::repositories::creation::insert_repository,
    fn insert_repository(pool: &DbPool, input: NewRepository<'_>) -> Result<Option<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::creation::repository_visibility_locked,
    crate::backend::db::sqlite::repositories::creation::repository_visibility_locked,
    fn repository_visibility_locked(pool: &DbPool, repo_id: i64) -> Result<bool>);
