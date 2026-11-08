use super::*;
use crate::organizations::teams::TeamRepositoryGrant;
use crate::organizations::OrganizationPage;
use crate::sqlite::organizations::teams::list_members;
use crate::sqlite::organizations::teams::list_repositories;
use codeyard_database::queries::organizations::OrganizationOutcome;

pub async fn list_direct_organization_team_members(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<TeamMembership>>> {
    list_members(pool, actor, org, team, page, true).await
}

pub async fn list_direct_organization_team_repositories(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<TeamRepositoryGrant>>> {
    list_repositories(pool, actor, org, team, page, true).await
}
