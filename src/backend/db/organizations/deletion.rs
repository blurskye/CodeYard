use super::OrganizationOutcome;
use crate::backend::db::DbPool;
use anyhow::Result;

// Only an active owner deletes an organization, and only after its repositories are gone.
// The result is Conflict while repositories remain. The API deletes them one by one through
// `next_organization_repository` first; `count_organization_repositories` says how many there are.
native_operation!(crate::backend::db::postgres::organizations::deletion::delete_organization, crate::backend::db::sqlite::organizations::deletion::delete_organization,
    fn delete_organization(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<()>>);
native_operation!(crate::backend::db::postgres::organizations::deletion::count_organization_repositories, crate::backend::db::sqlite::organizations::deletion::count_organization_repositories,
    fn count_organization_repositories(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<i64>>);
native_operation!(crate::backend::db::postgres::organizations::deletion::next_organization_repository, crate::backend::db::sqlite::organizations::deletion::next_organization_repository,
    fn next_organization_repository(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<Option<i64>>>);
