//! Shared named operation surface; native transactions stay inside db.
use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

/// An issue or a pull request, the two rows that can have assignees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignedItem {
    Issue(i64),
    PullRequest(i64),
}

/// The most people one issue or pull request can be assigned to, as GitHub allows.
pub const MAX_ASSIGNEES: i64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AssignmentRefusal {
    #[error("Assignment denied")]
    Denied,
    #[error("Assignment target not found")]
    NotFound,
    #[error("User cannot be assigned")]
    Unassignable,
    #[error("Too many assignees")]
    TooMany,
}

native_operation!(crate::backend::db::postgres::assignees::assign_users,
    crate::backend::db::sqlite::assignees::assign_users,
    /// Assigns each of `user_ids` in one transaction and returns the ones that were not assigned
    /// before. A user who is already assigned needs no access; a new assignee must be one that
    /// `assignable_user_ids` names.
    fn assign_users(pool: &DbPool, actor_id: impl Into<crate::backend::db::credentials::Actor>, repo_id: i64, item: AssignedItem, user_ids: &[i64]) -> Result<Vec<i64>>);

native_operation!(crate::backend::db::postgres::assignees::unassign_users,
    crate::backend::db::sqlite::assignees::unassign_users,
    /// Removes each user named in `usernames` in one transaction and returns the names that were
    /// assigned. The write right is checked before the names are looked up, so a name that is no
    /// user is refused the same way for a reader.
    fn unassign_users(pool: &DbPool, actor_id: impl Into<crate::backend::db::credentials::Actor>, repo_id: i64, item: AssignedItem, usernames: &[String]) -> Result<Vec<String>>);

/// Assigns one user. Returns whether that was a new assignment.
pub async fn assign_user(
    pool: &DbPool,
    actor_id: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    item: AssignedItem,
    user_id: i64,
) -> Result<bool> {
    Ok(!assign_users(pool, actor_id, repo_id, item, &[user_id])
        .await?
        .is_empty())
}

/// Removes the user named `username`. Returns whether an assignment was removed.
pub async fn unassign_user(
    pool: &DbPool,
    actor_id: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    item: AssignedItem,
    username: &str,
) -> Result<bool> {
    Ok(
        !unassign_users(pool, actor_id, repo_id, item, &[username.to_owned()])
            .await?
            .is_empty(),
    )
}

// The usernames assigned to one issue or pull request, in name order.
native_operation!(crate::backend::db::postgres::assignees::list_assignees,
    crate::backend::db::sqlite::assignees::list_assignees,
    fn list_assignees(pool: &DbPool, item: AssignedItem) -> Result<Vec<String>>);

// The assignees of many issues, or of many pull requests, in one query. Each row is an item id
// and a username, in name order. The callers list at most 100 items.
native_operation!(crate::backend::db::postgres::assignees::list_item_assignees,
    crate::backend::db::sqlite::assignees::list_item_assignees,
    fn list_item_assignees(pool: &DbPool, pull_requests: bool, item_ids: &[i64]) -> Result<Vec<(i64, String)>>);

native_operation!(crate::backend::db::postgres::assignees::count_assignable_users,
    crate::backend::db::sqlite::assignees::count_assignable_users,
    fn count_assignable_users(pool: &DbPool, repo_id: i64, query: &str) -> Result<i64>);
native_operation!(crate::backend::db::postgres::assignees::list_assignable_users,
    crate::backend::db::sqlite::assignees::list_assignable_users,
    fn list_assignable_users(pool: &DbPool, repo_id: i64, query: &str, offset: i64, limit: i64) -> Result<Vec<codeyard_db_core::models::User>>);

// The usernames that may be assigned in a repository: its personal owner and its collaborators.
native_operation!(crate::backend::db::postgres::assignees::list_assignable_usernames,
    crate::backend::db::sqlite::assignees::list_assignable_usernames,
    fn list_assignable_usernames(pool: &DbPool, repo_id: i64) -> Result<Vec<String>>);

native_operation!(crate::backend::db::postgres::assignees::assignable_user_ids,
    crate::backend::db::sqlite::assignees::assignable_user_ids,
    /// The users among `user_ids` who may be assigned in the repository.
    fn assignable_user_ids(pool: &DbPool, repo_id: i64, user_ids: &[i64]) -> Result<Vec<i64>>);
