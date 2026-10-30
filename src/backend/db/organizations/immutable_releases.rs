//! Organization enforcement is stored separately from repository release data.
use super::OrganizationOutcome;
use crate::backend::db::{models::Repository, DbPool};
use anyhow::Result;

pub struct Settings {
    pub enforced_repositories: String,
    /// Selected repositories in total, and the requested window of them.
    pub total: i64,
    pub repositories: Vec<Repository>,
}

pub enum Change {
    Enforcement(String, Option<Vec<i64>>),
    Repositories(Vec<i64>),
    Repository(i64, bool),
}

native_operation!(crate::backend::db::postgres::organizations::immutable_releases::get,
    crate::backend::db::sqlite::organizations::immutable_releases::get,
    fn get(pool: &DbPool, actor: i64, org: i64, offset: i64, limit: i64) -> Result<OrganizationOutcome<Settings>>);
native_operation!(crate::backend::db::postgres::organizations::immutable_releases::change,
    crate::backend::db::sqlite::organizations::immutable_releases::change,
    fn change(pool: &DbPool, actor: i64, org: i64, change: &Change) -> Result<OrganizationOutcome<()>>);
