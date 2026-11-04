//! Public organization discovery and membership visibility for REST and profile pages.
use super::OrganizationOutcome;
use anyhow::Result;
use codeyard_db_core::models::Organization;
use codeyard_db_core::models::User;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow)]
pub struct MembershipState {
    pub org_id: i64,
    pub user_id: i64,
    pub role: String,
    pub state: String,
}

native_operation!(crate::backend::db::postgres::organizations::discovery::list_organizations, crate::backend::db::sqlite::organizations::discovery::list_organizations,
    fn list_organizations(pool: &DbPool, user: Option<i64>, private: bool, since: i64, offset: i64, limit: i64) -> Result<Vec<Organization>>);
native_operation!(crate::backend::db::postgres::organizations::discovery::organization_public_repositories, crate::backend::db::sqlite::organizations::discovery::organization_public_repositories,
    fn organization_public_repositories(pool: &DbPool, org: i64) -> Result<i64>);
native_operation!(crate::backend::db::postgres::organizations::discovery::list_visible_members, crate::backend::db::sqlite::organizations::discovery::list_visible_members,
    fn list_visible_members(pool: &DbPool, actor: Option<i64>, org: i64, public: bool, role: &str, filter: &str, window: (i64, i64)) -> Result<OrganizationOutcome<Vec<User>>>);
native_operation!(crate::backend::db::postgres::organizations::discovery::list_membership_states, crate::backend::db::sqlite::organizations::discovery::list_membership_states,
    fn list_membership_states(pool: &DbPool, actor: i64, org: Option<i64>, target: i64, state: Option<&str>, window: (i64, i64)) -> Result<Vec<MembershipState>>);
native_operation!(crate::backend::db::postgres::organizations::discovery::set_membership_public, crate::backend::db::sqlite::organizations::discovery::set_membership_public,
    fn set_membership_public(pool: &DbPool, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64, public: bool) -> Result<OrganizationOutcome<()>>);
