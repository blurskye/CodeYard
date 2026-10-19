//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{AccessLevel, RepoAccess, Repository};
use crate::backend::db::DbPool;
use anyhow::Result;

native_operation!(crate::backend::db::postgres::repositories::access::readable_repositories_by_ids,
    crate::backend::db::sqlite::repositories::access::readable_repositories_by_ids,
    fn readable_repositories_by_ids(pool: &DbPool, user_id: i64, ids: &[i64]) -> Result<Vec<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::access::get_user_repo_access,
    crate::backend::db::sqlite::repositories::access::get_user_repo_access,
    fn get_user_repo_access(
    pool: &DbPool,
    user_id: i64,
    repo_id: i64,
) -> Result<AccessLevel>);

native_operation!(crate::backend::db::postgres::repositories::access::get_users_repo_access,
    crate::backend::db::sqlite::repositories::access::get_users_repo_access,
    fn get_users_repo_access(pool: &DbPool, user_ids: &[i64], repo_id: i64) -> Result<Vec<(i64, AccessLevel)>>);
native_operation!(crate::backend::db::postgres::repositories::access::readable_repository_ids,
    crate::backend::db::sqlite::repositories::access::readable_repository_ids,
    fn readable_repository_ids(pool: &DbPool, user_id: Option<i64>, repo_ids: &[i64]) -> Result<Vec<i64>>);

native_operation!(crate::backend::db::postgres::repositories::access::grant_repo_access,
    crate::backend::db::sqlite::repositories::access::grant_repo_access,
    fn grant_repo_access(
    pool: &DbPool,
    user_id: i64,
    repo_id: i64,
    level: AccessLevel,
) -> Result<RepoAccess>);

native_operation!(crate::backend::db::postgres::repositories::access::list_repo_collaborators,
    crate::backend::db::sqlite::repositories::access::list_repo_collaborators,
    fn list_repo_collaborators(pool: &DbPool, repo_id: i64) -> Result<Vec<RepoAccess>>);

native_operation!(crate::backend::db::postgres::repositories::access::write_access_user_ids,
    crate::backend::db::sqlite::repositories::access::write_access_user_ids,
    fn write_access_user_ids(pool: &DbPool, repo_id: i64, ids: &[i64]) -> Result<Vec<i64>>);

native_operation!(crate::backend::db::postgres::repositories::access::list_write_access_users,
    crate::backend::db::sqlite::repositories::access::list_write_access_users,
    fn list_write_access_users(pool: &DbPool, repo_id: i64, viewer_id: i64) -> Result<Vec<(String, AccessLevel)>>);

native_operation!(crate::backend::db::postgres::repositories::access::revoke_repo_access,
    crate::backend::db::sqlite::repositories::access::revoke_repo_access,
    fn revoke_repo_access(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<bool>);
