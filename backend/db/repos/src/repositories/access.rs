use anyhow::Result;
use codeyard_database::models::AccessLevel;
use codeyard_database::models::RepoAccess;
use codeyard_database::models::Repository;
use codeyard_database::models::User;
use codeyard_database::native_operation;
use codeyard_database::DbPool;

native_operation!(crate::postgres::repositories::access::git_access,
    crate::sqlite::repositories::access::git_access,
    fn git_access(pool: &DbPool, user_id: i64, repo_id: i64, write: bool) -> Result<AccessLevel>);

pub enum CollaboratorChange {
    Applied(Box<User>),
    NotFound,
    PasswordChangeRequired,
    InvalidCollaboratorOwner,
}

native_operation!(crate::postgres::repositories::access::change_personal_repository_collaborator,
    crate::sqlite::repositories::access::change_personal_repository_collaborator,
    fn change_personal_repository_collaborator(
    pool: &DbPool,
    actor: impl Into<codeyard_database::queries::credentials::Actor>,
    repo_id: i64,
    username: &str,
    permission: Option<AccessLevel>,
) -> Result<CollaboratorChange>);

native_operation!(crate::postgres::repositories::access::readable_repositories_by_ids,
    crate::sqlite::repositories::access::readable_repositories_by_ids,
    fn readable_repositories_by_ids(pool: &DbPool, user_id: i64, ids: &[i64]) -> Result<Vec<Repository>>);

native_operation!(crate::postgres::repositories::access::author_associations,
    crate::sqlite::repositories::access::author_associations,
    fn author_associations(pool: &DbPool, repo_id: i64, users: &[i64]) -> Result<Vec<(i64, String)>>);

native_operation!(crate::postgres::repositories::access::get_user_repo_access,
    crate::sqlite::repositories::access::get_user_repo_access,
    fn get_user_repo_access(
    pool: &DbPool,
    user_id: i64,
    repo_id: i64,
) -> Result<AccessLevel>);

native_operation!(crate::postgres::repositories::access::get_users_repo_access,
    crate::sqlite::repositories::access::get_users_repo_access,
    fn get_users_repo_access(pool: &DbPool, user_ids: &[i64], repo_id: i64) -> Result<Vec<(i64, AccessLevel)>>);
native_operation!(crate::postgres::repositories::access::readable_repository_ids,
    crate::sqlite::repositories::access::readable_repository_ids,
    fn readable_repository_ids(pool: &DbPool, user_id: Option<i64>, repo_ids: &[i64]) -> Result<Vec<i64>>);

native_operation!(crate::postgres::repositories::access::grant_repo_access,
    crate::sqlite::repositories::access::grant_repo_access,
    fn grant_repo_access(
    pool: &DbPool,
    user_id: i64,
    repo_id: i64,
    level: AccessLevel,
) -> Result<RepoAccess>);

native_operation!(crate::postgres::repositories::access::list_repo_collaborators,
    crate::sqlite::repositories::access::list_repo_collaborators,
    fn list_repo_collaborators(pool: &DbPool, repo_id: i64) -> Result<Vec<RepoAccess>>);

native_operation!(crate::postgres::repositories::access::write_access_user_ids,
    crate::sqlite::repositories::access::write_access_user_ids,
    fn write_access_user_ids(pool: &DbPool, repo_id: i64, ids: &[i64]) -> Result<Vec<i64>>);

native_operation!(crate::postgres::repositories::access::list_write_access_users,
    crate::sqlite::repositories::access::list_write_access_users,
    fn list_write_access_users(pool: &DbPool, repo_id: i64, viewer_id: i64) -> Result<Vec<(String, AccessLevel)>>);

native_operation!(crate::postgres::repositories::access::revoke_repo_access,
    crate::sqlite::repositories::access::revoke_repo_access,
    fn revoke_repo_access(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<bool>);
