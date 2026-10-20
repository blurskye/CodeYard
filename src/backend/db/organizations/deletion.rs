use super::OrganizationOutcome;
use crate::backend::db::DbPool;
use anyhow::Result;

// Only an active owner deletes an organization, and only after its repositories are gone.
// The result is Conflict while repositories remain. The API deletes them one by one through
// `list_organization_deletion_repositories` first; `count_organization_repositories` says how many there are.
native_operation!(crate::backend::db::postgres::organizations::deletion::delete_organization, crate::backend::db::sqlite::organizations::deletion::delete_organization,
    fn delete_organization(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<()>>);
native_operation!(crate::backend::db::postgres::organizations::deletion::count_organization_repositories, crate::backend::db::sqlite::organizations::deletion::count_organization_repositories,
    fn count_organization_repositories(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<i64>>);
native_operation!(crate::backend::db::postgres::organizations::deletion::list_organization_deletion_repositories, crate::backend::db::sqlite::organizations::deletion::list_organization_deletion_repositories,
    fn list_organization_deletion_repositories(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<Vec<i64>>>);
native_operation!(crate::backend::db::postgres::organizations::deletion::delete_organization_repository, crate::backend::db::sqlite::organizations::deletion::delete_organization_repository,
    fn delete_organization_repository(pool: &DbPool, actor: i64, org: i64, repo: i64) -> Result<OrganizationOutcome<crate::backend::db::models::Repository>>);
