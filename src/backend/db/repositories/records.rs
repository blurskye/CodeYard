//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{Repository, SharedRepository};
use crate::backend::db::DbPool;
use anyhow::Result;

native_operation!(crate::backend::db::postgres::repositories::records::create_repository,
    crate::backend::db::sqlite::repositories::records::create_repository,
    fn create_repository(
    pool: &DbPool,
    owner_id: i64,
    name: &str,
    description: Option<&str>,
    is_private: bool,
    disk_path: &str,
) -> Result<Repository>);

native_operation!(crate::backend::db::postgres::repositories::records::get_repository,
    crate::backend::db::sqlite::repositories::records::get_repository,
    fn get_repository(
    pool: &DbPool,
    owner_name: &str,
    repo_name: &str,
) -> Result<Option<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::records::get_repository_by_id,
    crate::backend::db::sqlite::repositories::records::get_repository_by_id,
    fn get_repository_by_id(pool: &DbPool, id: i64) -> Result<Option<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::records::get_repositories_by_ids,
    crate::backend::db::sqlite::repositories::records::get_repositories_by_ids,
    fn get_repositories_by_ids(pool: &DbPool, ids: &[i64]) -> Result<Vec<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::records::repository_owner_name,
    crate::backend::db::sqlite::repositories::records::repository_owner_name,
    /// Identity namespace only; callers must first authorize/scope the repository.
    fn repository_owner_name(pool: &DbPool, repo_id: i64) -> Result<Option<String>>);

native_operation!(crate::backend::db::postgres::repositories::records::list_user_repositories,
    crate::backend::db::sqlite::repositories::records::list_user_repositories,
    fn list_user_repositories(pool: &DbPool, user_id: i64) -> Result<Vec<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::records::list_shared_repositories,
    crate::backend::db::sqlite::repositories::records::list_shared_repositories,
    fn list_shared_repositories(pool: &DbPool, user_id: i64) -> Result<Vec<SharedRepository>>);

/// Order of a public repository list. The limit applies after this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositorySort {
    /// Newest first.
    Recent,
    /// Most stars first, then newest first.
    Stars,
}

native_operation!(crate::backend::db::postgres::repositories::records::list_public_repositories,
    crate::backend::db::sqlite::repositories::records::list_public_repositories,
    fn list_public_repositories(
    pool: &DbPool,
    sort: RepositorySort,
    limit: i64,
) -> Result<Vec<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::records::search_public_repositories,
    crate::backend::db::sqlite::repositories::records::search_public_repositories,
    fn search_public_repositories(
    pool: &DbPool,
    text: &str,
    sort: RepositorySort,
    limit: i64,
) -> Result<Vec<Repository>>);

/// Stable policy refusals; unrecognized database failures remain unknown errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RepositorySettingsRefusal {
    #[error("Repository settings denied")]
    Denied,
    #[error("Repository visibility is locked")]
    VisibilityLocked,
    #[error("Invalid repository name")]
    InvalidName,
    #[error("Repository name is already reserved")]
    NameConflict,
    #[error("At least one merge method must stay allowed")]
    NoMergeMethod,
}

pub(crate) fn validate_repository_alias(name: &str) -> Result<()> {
    // Match the existing accepted byte bound, retaining Unicode alphanumerics.
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 100
            && name != "."
            && name != ".."
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')),
        RepositorySettingsRefusal::InvalidName
    );
    Ok(())
}

native_operation!(crate::backend::db::postgres::repositories::records::update_repository,
    crate::backend::db::sqlite::repositories::records::update_repository,
    /// Saves the description, visibility and merge methods in one transaction. A refusal or a
    /// failed write leaves all three unchanged.
    fn update_repository(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    description: Option<&str>,
    is_private: bool,
    merge_methods: MergeMethodUpdate,
) -> Result<()>);

/// A change to the merge methods that pull requests may use. A method left as `None` keeps
/// its stored value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeMethodUpdate {
    pub merge: Option<bool>,
    pub squash: Option<bool>,
    pub rebase: Option<bool>,
}

native_operation!(crate::backend::db::postgres::repositories::records::set_merge_methods,
    crate::backend::db::sqlite::repositories::records::set_merge_methods,
    fn set_merge_methods(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    update: MergeMethodUpdate,
) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::records::update_repo_default_branch_and_size,
    crate::backend::db::sqlite::repositories::records::update_repo_default_branch_and_size,
    fn update_repo_default_branch_and_size(
    pool: &DbPool,
    repo_id: i64,
    default_branch: &str,
    size_bytes: i64,
) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::records::insert_repo_storage_snapshot,
    crate::backend::db::sqlite::repositories::records::insert_repo_storage_snapshot,
    fn insert_repo_storage_snapshot(
    pool: &DbPool,
    repo_id: i64,
    size_bytes: i64,
) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::records::delete_repository,
    crate::backend::db::sqlite::repositories::records::delete_repository,
    fn delete_repository(pool: &DbPool, actor_id: i64, repo_id: i64) -> Result<Repository>);

native_operation!(crate::backend::db::postgres::repositories::records::rename_repository,
    crate::backend::db::sqlite::repositories::records::rename_repository,
    fn rename_repository(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    new_name: &str,
) -> Result<Repository>);

native_operation!(crate::backend::db::postgres::repositories::records::transfer_repository,
    crate::backend::db::sqlite::repositories::records::transfer_repository,
    fn transfer_repository(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    target: crate::backend::db::repositories::creation::RepositoryTarget,
) -> Result<Repository>);

/// Canonical namespaces for already-selected repositories. This display read does
/// not grant access; callers retain their visibility filtering before page limits.
pub async fn repository_owner_names(
    pool: &DbPool,
    repository_ids: &[i64],
) -> Result<std::collections::BTreeMap<i64, String>> {
    let mut ids = repository_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut owners = std::collections::BTreeMap::new();
    for chunk in ids.chunks(500) {
        let rows = match pool {
            DbPool::Postgres(pool) => {
                crate::backend::db::postgres::repositories::records::repository_owner_names(
                    pool, chunk,
                )
                .await?
            }
            DbPool::Sqlite(pool) => {
                crate::backend::db::sqlite::repositories::records::repository_owner_names(
                    pool, chunk,
                )
                .await?
            }
        };
        owners.extend(rows);
    }
    Ok(owners)
}
