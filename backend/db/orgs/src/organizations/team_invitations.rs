use super::TeamRole;
use crate::organizations::OrganizationPage;
use anyhow::Result;
use codeyard_db_core::models::OrgInvitation;
use codeyard_db_core::native_operation;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::DbPool;

#[derive(Debug, sqlx::FromRow)]
pub struct TeamInvitation {
    #[sqlx(flatten)]
    pub invitation: OrgInvitation,
    pub team_count: i64,
}

native_operation!(crate::postgres::organizations::team_invitations::invite_team_member, crate::sqlite::organizations::team_invitations::invite_team_member,
    fn invite_team_member(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, org: i64, team: i64, target: i64, role: TeamRole) -> Result<OrganizationOutcome<OrgInvitation>>);
native_operation!(crate::postgres::organizations::team_invitations::pending_team_membership, crate::sqlite::organizations::team_invitations::pending_team_membership,
    fn pending_team_membership(pool: &DbPool, actor: i64, org: i64, team: i64, target: i64) -> Result<Option<String>>);
native_operation!(crate::postgres::organizations::team_invitations::list_team_invitations, crate::sqlite::organizations::team_invitations::list_team_invitations,
    fn list_team_invitations(pool: &DbPool, actor: i64, org: i64, team: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<TeamInvitation>>>);
