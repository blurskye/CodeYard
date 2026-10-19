//! Organization invitations: an owner or admin offers membership, and only the invitee's
//! acceptance adds the member.
use super::*;
use crate::backend::db::{
    models::{OrgInvitation, OrgMembership, OrgRole, ReceivedInvitation},
    DbPool,
};
use anyhow::Result;

native_operation!(crate::backend::db::postgres::organizations::invitations::invite_organization_member, crate::backend::db::sqlite::organizations::invitations::invite_organization_member,
    fn invite_organization_member(pool: &DbPool, actor: i64, org: i64, target: i64, role: OrgRole) -> Result<OrganizationOutcome<OrgInvitation>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::list_organization_invitations, crate::backend::db::sqlite::organizations::invitations::list_organization_invitations,
    fn list_organization_invitations(pool: &DbPool, actor: i64, org: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<OrgInvitation>>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::cancel_organization_invitation, crate::backend::db::sqlite::organizations::invitations::cancel_organization_invitation,
    fn cancel_organization_invitation(pool: &DbPool, actor: i64, org: i64, target: i64) -> Result<OrganizationOutcome<()>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::list_received_organization_invitations, crate::backend::db::sqlite::organizations::invitations::list_received_organization_invitations,
    fn list_received_organization_invitations(pool: &DbPool, actor: i64) -> Result<Vec<ReceivedInvitation>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::accept_organization_invitation, crate::backend::db::sqlite::organizations::invitations::accept_organization_invitation,
    fn accept_organization_invitation(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<OrgMembership>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::decline_organization_invitation, crate::backend::db::sqlite::organizations::invitations::decline_organization_invitation,
    fn decline_organization_invitation(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<()>>);
