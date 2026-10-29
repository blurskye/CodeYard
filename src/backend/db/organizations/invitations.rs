//! Organization invitations: an owner or admin offers membership, and only the invitee's
//! acceptance adds the member.
use super::*;
use crate::backend::db::{
    models::{OrgInvitation, OrgMembership, OrgRole, ReceivedInvitation},
    DbPool,
};
use anyhow::Result;

/// Native and REST offers use the same recipient and team validation in the authority transaction.
#[derive(Debug, Clone)]
pub struct NewInvitation {
    pub user_id: Option<i64>,
    pub email: Option<String>,
    pub role: OrgRole,
    pub reinstate: bool,
    pub teams: Option<InvitationTeams>,
}
#[derive(Clone, Copy)]
pub(in crate::backend::db) enum InvitationOffer {
    Create,
    Replace,
    Retain,
}

#[derive(Debug, Clone)]
pub enum InvitationTeams {
    Ids(Vec<i64>),
    Slugs(Vec<String>),
}

impl NewInvitation {
    pub fn user(user_id: i64, role: OrgRole) -> Self {
        Self {
            user_id: Some(user_id),
            email: None,
            role,
            reinstate: false,
            teams: None,
        }
    }
    pub(crate) fn valid(&self) -> bool {
        if self.user_id.is_some() == self.email.is_some() || self.user_id.is_some_and(|id| id <= 0)
        {
            return false;
        }
        if let Some(email) = &self.email {
            if email.len() > 255
                || email != &email.trim().to_ascii_lowercase()
                || email.contains(['\r', '\n', '\0'])
                || email.parse::<lettre::Address>().is_err()
            {
                return false;
            }
        }
        match &self.teams {
            None => true,
            Some(InvitationTeams::Ids(ids)) => ids.len() <= 1000 && ids.iter().all(|id| *id > 0),
            Some(InvitationTeams::Slugs(names)) => {
                // Team handles follow the rules team creation uses.
                names.len() <= 1000
                    && names
                        .iter()
                        .all(|name| super::profiles::validate_slug(name).is_ok())
            }
        }
    }
}

#[derive(Default)]
pub struct InvitationFilter {
    pub failed: bool,
    pub role: Option<OrgRole>,
    pub empty: bool,
    pub owner_only: bool,
    pub after: i64,
}

native_operation!(crate::backend::db::postgres::organizations::invitations::create_organization_invitation, crate::backend::db::sqlite::organizations::invitations::create_organization_invitation,
    fn create_organization_invitation(pool: &DbPool, actor: impl Into<crate::backend::db::users::tokens::Actor>, org: i64, input: &NewInvitation, owner_only: bool) -> Result<OrganizationOutcome<OrgInvitation>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::invitation_page, crate::backend::db::sqlite::organizations::invitations::invitation_page,
    fn invitation_page(pool: &DbPool, actor: i64, org: i64, filter: &InvitationFilter, window: (i64,i64)) -> Result<OrganizationOutcome<Vec<OrgInvitation>>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::invitation_teams, crate::backend::db::sqlite::organizations::invitations::invitation_teams,
    fn invitation_teams(pool: &DbPool, actor: i64, org: i64, id: i64, window: (i64,i64)) -> Result<OrganizationOutcome<Vec<crate::backend::db::models::Team>>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::invitation_team_counts, crate::backend::db::sqlite::organizations::invitations::invitation_team_counts,
    fn invitation_team_counts(pool: &DbPool, ids: &[i64]) -> Result<std::collections::HashMap<i64,i64>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::cancel_invitation_id, crate::backend::db::sqlite::organizations::invitations::cancel_invitation_id,
    fn cancel_invitation_id(pool: &DbPool, actor: impl Into<crate::backend::db::users::tokens::Actor>, org: i64, id: i64, owner_only: bool) -> Result<OrganizationOutcome<()>>);

native_operation!(crate::backend::db::postgres::organizations::invitations::invite_organization_member, crate::backend::db::sqlite::organizations::invitations::invite_organization_member,
    fn invite_organization_member(pool: &DbPool, actor: i64, org: i64, target: i64, role: OrgRole) -> Result<OrganizationOutcome<OrgInvitation>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::set_organization_invitation_by_owner, crate::backend::db::sqlite::organizations::invitations::set_organization_invitation_by_owner,
    fn set_organization_invitation_by_owner(pool: &DbPool, actor: impl Into<crate::backend::db::users::tokens::Actor>, org: i64, target: i64, role: OrgRole) -> Result<OrganizationOutcome<OrgInvitation>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::list_organization_invitations, crate::backend::db::sqlite::organizations::invitations::list_organization_invitations,
    fn list_organization_invitations(pool: &DbPool, actor: i64, org: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<OrgInvitation>>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::cancel_organization_invitation, crate::backend::db::sqlite::organizations::invitations::cancel_organization_invitation,
    fn cancel_organization_invitation(pool: &DbPool, actor: i64, org: i64, target: i64) -> Result<OrganizationOutcome<()>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::list_received_organization_invitations, crate::backend::db::sqlite::organizations::invitations::list_received_organization_invitations,
    fn list_received_organization_invitations(pool: &DbPool, actor: i64, page: OrganizationPage) -> Result<Vec<ReceivedInvitation>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::accept_organization_invitation, crate::backend::db::sqlite::organizations::invitations::accept_organization_invitation,
    fn accept_organization_invitation(pool: &DbPool, actor: i64, org: i64, invitation: Option<i64>) -> Result<OrganizationOutcome<OrgMembership>>);
native_operation!(crate::backend::db::postgres::organizations::invitations::decline_organization_invitation, crate::backend::db::sqlite::organizations::invitations::decline_organization_invitation,
    fn decline_organization_invitation(pool: &DbPool, actor: i64, org: i64, invitation: Option<i64>) -> Result<OrganizationOutcome<()>>);
