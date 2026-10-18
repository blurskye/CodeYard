use super::*;
use crate::backend::db::{
    models::{OrgMembership, OrgRole},
    DbPool,
};
use anyhow::Result;

native_operation!(crate::backend::db::postgres::organizations::membership::list_organization_members, crate::backend::db::sqlite::organizations::membership::list_organization_members,
    fn list_organization_members(pool: &DbPool, actor: i64, org: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<OrgMembership>>>);
native_operation!(crate::backend::db::postgres::organizations::membership::add_organization_member, crate::backend::db::sqlite::organizations::membership::add_organization_member,
    fn add_organization_member(pool: &DbPool, actor: i64, org: i64, target: i64, role: OrgRole) -> Result<OrganizationOutcome<OrgMembership>>);
// Inactive owners and admins may only be downgraded to member, and only by an active owner.
native_operation!(crate::backend::db::postgres::organizations::membership::update_organization_member_role, crate::backend::db::sqlite::organizations::membership::update_organization_member_role,
    fn update_organization_member_role(pool: &DbPool, actor: i64, org: i64, target: i64, role: OrgRole) -> Result<OrganizationOutcome<OrgMembership>>);
native_operation!(crate::backend::db::postgres::organizations::membership::remove_organization_member, crate::backend::db::sqlite::organizations::membership::remove_organization_member,
    fn remove_organization_member(pool: &DbPool, actor: i64, org: i64, target: i64) -> Result<OrganizationOutcome<()>>);

// The viewer's own role, which chooses the controls a page shows. Every write rechecks it.
native_operation!(crate::backend::db::postgres::organizations::membership::get_organization_role, crate::backend::db::sqlite::organizations::membership::get_organization_role,
    fn get_organization_role(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<OrgRole>>);
// Member count for readers of the organization; anyone else is refused.
native_operation!(crate::backend::db::postgres::organizations::membership::count_organization_members, crate::backend::db::sqlite::organizations::membership::count_organization_members,
    fn count_organization_members(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<i64>>);

pub fn parse_organization_role(
    value: &str,
) -> std::result::Result<OrgRole, OrganizationInputError> {
    value.parse().map_err(|()| OrganizationInputError::Role)
}
