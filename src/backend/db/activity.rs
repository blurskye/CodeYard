//! Profile activity: branch pushes, and what a user opened, merged or commented on.
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

/// One item of a user's activity. `kind` is one of `push`, `repository_created`,
/// `issue_opened`, `pull_request_opened`, `pull_request_merged`, `issue_comment` or
/// `pull_request_comment`. `number`, `title` and `branch` are set only for the kinds that
/// have them.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ActivityRow {
    pub kind: String,
    pub item_id: i64,
    pub repo_id: i64,
    pub created_at: DateTime<Utc>,
    pub number: Option<i32>,
    pub title: Option<String>,
    pub branch: Option<String>,
    pub commit_count: Option<i32>,
    pub owner_name: String,
    pub repo_name: String,
}

native_operation!(crate::backend::db::postgres::activity::record_repository_push,
    crate::backend::db::sqlite::activity::record_repository_push,
    fn record_repository_push(
    pool: &DbPool,
    repo_id: i64,
    actor_id: i64,
    branch: &str,
    commit_count: i32,
) -> Result<()>);

native_operation!(crate::backend::db::postgres::activity::list_user_activity,
    crate::backend::db::sqlite::activity::list_user_activity,
    fn list_user_activity(pool: &DbPool, user_id: i64, viewer: Option<i64>, limit: i64, offset: i64) -> Result<Vec<ActivityRow>>);
