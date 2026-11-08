use crate::organizations::OrganizationInputError;
use crate::organizations::OrganizationPage;
use anyhow::Result;
use codeyard_database::models::OrgMembership;
use codeyard_database::models::OrgRole;
use codeyard_database::native_operation;
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_database::DbPool;

native_operation!(crate::postgres::organizations::membership::list_organization_members, crate::sqlite::organizations::membership::list_organization_members,
    fn list_organization_members(pool: &DbPool, actor: i64, org: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<OrgMembership>>>);
native_operation!(crate::postgres::organizations::membership::add_organization_member, crate::sqlite::organizations::membership::add_organization_member,
    fn add_organization_member(pool: &DbPool, actor: i64, org: i64, target: i64, role: OrgRole) -> Result<OrganizationOutcome<OrgMembership>>);
// Inactive owners and admins may only be downgraded to member, and only by an active owner.
native_operation!(crate::postgres::organizations::membership::update_organization_member_role, crate::sqlite::organizations::membership::update_organization_member_role,
    fn update_organization_member_role(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, target: i64, role: OrgRole) -> Result<OrganizationOutcome<OrgMembership>>);
native_operation!(crate::postgres::organizations::membership::remove_organization_member, crate::sqlite::organizations::membership::remove_organization_member,
    fn remove_organization_member(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, target: i64) -> Result<OrganizationOutcome<()>>);

// The viewer's own role, which chooses the controls a page shows. Every write rechecks it.
native_operation!(crate::postgres::organizations::membership::get_organization_role, crate::sqlite::organizations::membership::get_organization_role,
    fn get_organization_role(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<OrgRole>>);
// Member count for readers of the organization; anyone else is refused.
native_operation!(crate::postgres::organizations::membership::count_organization_members, crate::sqlite::organizations::membership::count_organization_members,
    fn count_organization_members(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<i64>>);

// REST's admin role is an owner, not Codeyard's delegated administrator role.
native_operation!(crate::postgres::organizations::membership::update_organization_member_role_by_owner, crate::sqlite::organizations::membership::update_organization_member_role_by_owner,
    fn update_organization_member_role_by_owner(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, target: i64, role: OrgRole) -> Result<OrganizationOutcome<OrgMembership>>);
// Ends the membership or, for a non-member, cancels the pending invitations, in one transaction.
native_operation!(crate::postgres::organizations::membership::remove_member_or_invitation_by_owner, crate::sqlite::organizations::membership::remove_member_or_invitation_by_owner,
    fn remove_member_or_invitation_by_owner(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, target: i64) -> Result<OrganizationOutcome<()>>);

native_operation!(crate::postgres::organizations::membership::convert_to_outside_collaborator, crate::sqlite::organizations::membership::convert_to_outside_collaborator,
    fn convert_to_outside_collaborator(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, target: i64) -> Result<OrganizationOutcome<()>>);

pub fn parse_organization_role(
    value: &str,
) -> std::result::Result<OrgRole, OrganizationInputError> {
    value.parse().map_err(|()| OrganizationInputError::Role)
}
