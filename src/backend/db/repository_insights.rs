//! Repository Insights counts and lists the database answers: pull requests and issues opened,
//! merged or closed since a time. Commit history comes from Git.
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

/// Exact counts of the pull requests and issues that changed since the window started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct ActivityCounts {
    pub merged_pulls: i64,
    pub opened_pulls: i64,
    pub closed_issues: i64,
    pub opened_issues: i64,
}

/// A pull request or issue in a list, with the time that put it there.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NumberedItem {
    pub number: i32,
    pub title: String,
    pub at: DateTime<Utc>,
}

native_operation!(crate::backend::db::postgres::repository_insights::count_activity,
    crate::backend::db::sqlite::repository_insights::count_activity,
    fn count_activity(pool: &DbPool, repo_id: i64, since: DateTime<Utc>) -> Result<ActivityCounts>);

native_operation!(crate::backend::db::postgres::repository_insights::list_merged_pulls,
    crate::backend::db::sqlite::repository_insights::list_merged_pulls,
    fn list_merged_pulls(pool: &DbPool, repo_id: i64, since: DateTime<Utc>, limit: i64) -> Result<Vec<NumberedItem>>);

native_operation!(crate::backend::db::postgres::repository_insights::list_opened_issues,
    crate::backend::db::sqlite::repository_insights::list_opened_issues,
    fn list_opened_issues(pool: &DbPool, repo_id: i64, since: DateTime<Utc>, limit: i64) -> Result<Vec<NumberedItem>>);
