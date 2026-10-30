//! Direct edges for migration archives; effective memberships must not become new grants.
use super::*;

pub async fn list_direct_organization_team_members(
    pool: &PgPool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<TeamMembership>>> {
    list_members(pool, actor, org, team, page, true).await
}

pub async fn list_direct_organization_team_repositories(
    pool: &PgPool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<TeamRepositoryGrant>>> {
    list_repositories(pool, actor, org, team, page, true).await
}
