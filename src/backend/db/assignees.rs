//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::DbPool;
use anyhow::Result;

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

native_operation!(crate::backend::db::postgres::assignees::assign_user,
    crate::backend::db::sqlite::assignees::assign_user,
    fn assign_user(pool: &DbPool, actor_id: i64, repo_id: i64, item: AssignedItem, user_id: i64) -> Result<bool>);

// Removes the user named `username` from the item. Returns whether an assignment was removed. The
// write right is checked before the name is looked up, so a name that is no user is refused the same
// way for a reader.
native_operation!(crate::backend::db::postgres::assignees::unassign_user,
    crate::backend::db::sqlite::assignees::unassign_user,
    fn unassign_user(pool: &DbPool, actor_id: i64, repo_id: i64, item: AssignedItem, username: &str) -> Result<bool>);

// The usernames assigned to one issue or pull request, in name order.
native_operation!(crate::backend::db::postgres::assignees::list_assignees,
    crate::backend::db::sqlite::assignees::list_assignees,
    fn list_assignees(pool: &DbPool, item: AssignedItem) -> Result<Vec<String>>);

// The usernames that may be assigned in a repository: its personal owner and its collaborators.
// The assignees of many issues, or of many pull requests, in one query. Each row is an item id
// and a username, in name order. The callers list at most 100 items.
native_operation!(crate::backend::db::postgres::assignees::list_item_assignees,
    crate::backend::db::sqlite::assignees::list_item_assignees,
    fn list_item_assignees(pool: &DbPool, pull_requests: bool, item_ids: &[i64]) -> Result<Vec<(i64, String)>>);

native_operation!(crate::backend::db::postgres::assignees::list_assignable_usernames,
    crate::backend::db::sqlite::assignees::list_assignable_usernames,
    fn list_assignable_usernames(pool: &DbPool, repo_id: i64) -> Result<Vec<String>>);
