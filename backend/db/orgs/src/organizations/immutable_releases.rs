use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::queries::repository_stats::RepositorySnapshot;
use codeyard_db_core::DbPool;

pub struct Settings {
    pub enforced_repositories: String,
}

pub enum Change {
    Enforcement(String, Option<Vec<i64>>),
    Repositories(Vec<i64>),
    Repository(i64, bool),
}

native_operation!(crate::postgres::organizations::immutable_releases::get,
    crate::sqlite::organizations::immutable_releases::get,
    fn get(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<Settings>>);
native_operation!(crate::postgres::organizations::immutable_releases::repositories,
    crate::sqlite::organizations::immutable_releases::repositories,
    fn repositories(pool: &DbPool, actor: i64, org: i64, offset: i64, limit: i64) -> Result<OrganizationOutcome<(i64, Vec<RepositorySnapshot>)>>);
native_operation!(crate::postgres::organizations::immutable_releases::change,
    crate::sqlite::organizations::immutable_releases::change,
    fn change(pool: &DbPool, actor: i64, org: i64, change: &Change) -> Result<OrganizationOutcome<()>>);
