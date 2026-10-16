//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{AccessLevel, RepoAccess};
use crate::backend::db::DbPool;
use anyhow::Result;

native_operation!(crate::backend::db::postgres::repositories::access::get_user_repo_access,
    crate::backend::db::sqlite::repositories::access::get_user_repo_access,
    fn get_user_repo_access(
    pool: &DbPool,
    user_id: i64,
    repo_id: i64,
) -> Result<AccessLevel>);

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

native_operation!(crate::backend::db::postgres::repositories::access::list_write_access_users,
    crate::backend::db::sqlite::repositories::access::list_write_access_users,
    fn list_write_access_users(pool: &DbPool, repo_id: i64, viewer_id: i64) -> Result<Vec<(String, AccessLevel)>>);

native_operation!(crate::backend::db::postgres::repositories::access::revoke_repo_access,
    crate::backend::db::sqlite::repositories::access::revoke_repo_access,
    fn revoke_repo_access(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<bool>);
