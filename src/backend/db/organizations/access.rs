use super::{OrganizationOutcome, OrganizationPage};
use crate::backend::db::{
    models::{AccessLevel, Repository},
    DbPool,
};
use anyhow::Result;

// Display only; effectful callers recheck access in their native transaction.
native_operation!(crate::backend::db::postgres::organizations::access::get_organization_repo_access, crate::backend::db::sqlite::organizations::access::get_organization_repo_access,
    fn get_organization_repo_access(pool: &DbPool, user: i64, repository: &Repository) -> Result<AccessLevel>);
// Repositories the actor may read, for choosing a team grant. The filter is the one
// list_organization_team_repositories uses, so private repositories stay hidden without READ.
native_operation!(crate::backend::db::postgres::organizations::access::list_organization_repositories, crate::backend::db::sqlite::organizations::access::list_organization_repositories,
    fn list_organization_repositories(pool: &DbPool, actor: i64, org: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<OrganizationRepository>>>);

// Public repositories for everyone; private ones only for a viewer who may read them.
// An anonymous viewer passes `None`.
native_operation!(crate::backend::db::postgres::organizations::access::list_organization_profile_repositories, crate::backend::db::sqlite::organizations::access::list_organization_profile_repositories,
    fn list_organization_profile_repositories(pool: &DbPool, org: i64, viewer: Option<i64>, page: OrganizationPage) -> Result<Vec<Repository>>);

native_operation!(crate::backend::db::postgres::organizations::access::count_organization_profile_repositories, crate::backend::db::sqlite::organizations::access::count_organization_profile_repositories,
    fn count_organization_profile_repositories(pool: &DbPool, org: i64, viewer: Option<i64>) -> Result<i64>);

#[derive(Debug, sqlx::FromRow)]
pub struct OrganizationRepository {
    pub id: i64,
    pub owner: String,
    pub name: String,
}
