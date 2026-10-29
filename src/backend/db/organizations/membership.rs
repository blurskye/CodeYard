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

// REST's admin role is an owner, not Codeyard's delegated administrator role.
native_operation!(crate::backend::db::postgres::organizations::membership::update_organization_member_role_by_owner, crate::backend::db::sqlite::organizations::membership::update_organization_member_role_by_owner,
    fn update_organization_member_role_by_owner(pool: &DbPool, actor: impl Into<crate::backend::db::users::tokens::Actor>, org: i64, target: i64, role: OrgRole) -> Result<OrganizationOutcome<OrgMembership>>);
// Ends the membership or, for a non-member, cancels the pending invitations, in one transaction.
native_operation!(crate::backend::db::postgres::organizations::membership::remove_member_or_invitation_by_owner, crate::backend::db::sqlite::organizations::membership::remove_member_or_invitation_by_owner,
    fn remove_member_or_invitation_by_owner(pool: &DbPool, actor: impl Into<crate::backend::db::users::tokens::Actor>, org: i64, target: i64) -> Result<OrganizationOutcome<()>>);

native_operation!(crate::backend::db::postgres::organizations::membership::convert_to_outside_collaborator, crate::backend::db::sqlite::organizations::membership::convert_to_outside_collaborator,
    fn convert_to_outside_collaborator(pool: &DbPool, actor: impl Into<crate::backend::db::users::tokens::Actor>, org: i64, target: i64) -> Result<OrganizationOutcome<()>>);
native_operation!(crate::backend::db::postgres::organizations::membership::prepare_outside_conversion, crate::backend::db::sqlite::organizations::membership::prepare_outside_conversion,
    fn prepare_outside_conversion(pool: &DbPool, actor: i64, org: i64, target: i64) -> Result<OrganizationOutcome<()>>);

pub fn parse_organization_role(
    value: &str,
) -> std::result::Result<OrgRole, OrganizationInputError> {
    value.parse().map_err(|()| OrganizationInputError::Role)
}
