//! Organization enforcement is stored separately from repository release data.
use super::OrganizationOutcome;
use crate::backend::db::repository_stats::RepositorySnapshot;
use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

pub struct Settings {
    pub enforced_repositories: String,
}

pub enum Change {
    Enforcement(String, Option<Vec<i64>>),
    Repositories(Vec<i64>),
    Repository(i64, bool),
}

native_operation!(crate::backend::db::postgres::organizations::immutable_releases::get,
    crate::backend::db::sqlite::organizations::immutable_releases::get,
    fn get(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<Settings>>);
native_operation!(crate::backend::db::postgres::organizations::immutable_releases::repositories,
    crate::backend::db::sqlite::organizations::immutable_releases::repositories,
    fn repositories(pool: &DbPool, actor: i64, org: i64, offset: i64, limit: i64) -> Result<OrganizationOutcome<(i64, Vec<RepositorySnapshot>)>>);
native_operation!(crate::backend::db::postgres::organizations::immutable_releases::change,
    crate::backend::db::sqlite::organizations::immutable_releases::change,
    fn change(pool: &DbPool, actor: i64, org: i64, change: &Change) -> Result<OrganizationOutcome<()>>);
