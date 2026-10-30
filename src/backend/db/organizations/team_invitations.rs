//! A team invitation joins the organization's existing acceptance and cancellation flow.
use super::{OrganizationOutcome, OrganizationPage, TeamRole};
use crate::backend::db::{models::OrgInvitation, DbPool};
use anyhow::Result;

#[derive(Debug, sqlx::FromRow)]
pub struct TeamInvitation {
    #[sqlx(flatten)]
    pub invitation: OrgInvitation,
    pub team_count: i64,
}

native_operation!(crate::backend::db::postgres::organizations::team_invitations::invite_team_member, crate::backend::db::sqlite::organizations::team_invitations::invite_team_member,
    fn invite_team_member(pool: &DbPool, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64, team: i64, target: i64, role: TeamRole) -> Result<OrganizationOutcome<OrgInvitation>>);
native_operation!(crate::backend::db::postgres::organizations::team_invitations::pending_team_membership, crate::backend::db::sqlite::organizations::team_invitations::pending_team_membership,
    fn pending_team_membership(pool: &DbPool, actor: i64, org: i64, team: i64, target: i64) -> Result<Option<String>>);
native_operation!(crate::backend::db::postgres::organizations::team_invitations::list_team_invitations, crate::backend::db::sqlite::organizations::team_invitations::list_team_invitations,
    fn list_team_invitations(pool: &DbPool, actor: i64, org: i64, team: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<TeamInvitation>>>);
