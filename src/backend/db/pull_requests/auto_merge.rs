//! Auto-merge requests: pull requests that merge once their requirements pass.
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AutoMergeRequest {
    pub pull_request_id: i64,
    pub enabled_by: i64,
    pub token_id: Option<i64>,
    pub merge_method: String,
    pub commit_title: Option<String>,
    pub commit_message: Option<String>,
    pub author_email: Option<String>,
    pub enabled_at: DateTime<Utc>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AutoMerge {
    #[sqlx(flatten)]
    pub request: AutoMergeRequest,
    /// Renewed by storage on every enable: timestamps cannot identify consent.
    pub generation: String,
}

pub async fn enable_auto_merge(pool: &DbPool, request: &AutoMergeRequest) -> Result<()> {
    enable_auto_merge_at_head(pool, request, None).await
}
native_operation!(crate::backend::db::postgres::pull_requests::auto_merge::enable,
    crate::backend::db::sqlite::pull_requests::auto_merge::enable,
    /// Saves the request under live write authority and the supplied head revision, replacing an
    /// earlier one for the same pull request. A token request needs its contents and pull-requests grants.
    fn enable_auto_merge_at_head(pool: &DbPool, request: &AutoMergeRequest, expected_head: Option<&str>) -> Result<()>);
native_operation!(crate::backend::db::postgres::pull_requests::auto_merge::disable,
    crate::backend::db::sqlite::pull_requests::auto_merge::disable,
    /// Removes the request. True when there was one.
    fn disable_auto_merge(pool: &DbPool, pull_request_id: i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::pull_requests::auto_merge::retire,
    crate::backend::db::sqlite::pull_requests::auto_merge::retire,
    /// Retires only the exact request inspected by the worker, never its replacement.
    fn retire_auto_merge(pool: &DbPool, pull_request_id: i64, generation: &str) -> Result<bool>);
native_operation!(crate::backend::db::postgres::pull_requests::auto_merge::get,
    crate::backend::db::sqlite::pull_requests::auto_merge::get,
    fn get_auto_merge(pool: &DbPool, pull_request_id: i64) -> Result<Option<AutoMerge>>);
native_operation!(crate::backend::db::postgres::pull_requests::auto_merge::pending,
    crate::backend::db::sqlite::pull_requests::auto_merge::pending,
    /// Requests after the given pull request id, in id order, at most `limit`.
    fn pending_auto_merges(pool: &DbPool, after: i64, limit: i64) -> Result<Vec<AutoMerge>>);
