use super::profiles::{validate_description, validate_name, validate_slug};
use super::*;
use crate::backend::db::{
    models::{Team, TeamMembership, TeamRepoAccess},
    DbPool,
};
use anyhow::Result;

native_operation!(crate::backend::db::postgres::organizations::teams::create_organization_team, crate::backend::db::sqlite::organizations::teams::create_organization_team,
    fn create_organization_team(pool: &DbPool, actor: i64, org: i64, input: &NewTeam) -> Result<OrganizationOutcome<Team>>);
native_operation!(crate::backend::db::postgres::organizations::teams::update_organization_team, crate::backend::db::sqlite::organizations::teams::update_organization_team,
    fn update_organization_team(pool: &DbPool, actor: i64, org: i64, team: i64, input: &TeamProfile) -> Result<OrganizationOutcome<Team>>);
native_operation!(crate::backend::db::postgres::organizations::teams::delete_organization_team, crate::backend::db::sqlite::organizations::teams::delete_organization_team,
    fn delete_organization_team(pool: &DbPool, actor: i64, org: i64, team: i64) -> Result<OrganizationOutcome<()>>);
native_operation!(crate::backend::db::postgres::organizations::teams::list_organization_teams, crate::backend::db::sqlite::organizations::teams::list_organization_teams,
    fn list_organization_teams(pool: &DbPool, actor: i64, org: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<TeamListing>>>);
// Teams a reader may see, counted the same way the public team list shows them.
native_operation!(crate::backend::db::postgres::organizations::teams::count_organization_teams, crate::backend::db::sqlite::organizations::teams::count_organization_teams,
    fn count_organization_teams(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<i64>>);
// Code owner names such as `@org/team` resolve to these users; no actor, since a merge judges them.
native_operation!(crate::backend::db::postgres::organizations::teams::team_members_by_names, crate::backend::db::sqlite::organizations::teams::team_members_by_names,
    fn team_members_by_names(pool: &DbPool, org_id: i64, teams: &[String]) -> Result<Vec<(String, i64)>>);
native_operation!(crate::backend::db::postgres::organizations::teams::list_organization_team_members, crate::backend::db::sqlite::organizations::teams::list_organization_team_members,
    fn list_organization_team_members(pool: &DbPool, actor: i64, org: i64, team: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<TeamMembership>>>);
native_operation!(crate::backend::db::postgres::organizations::teams::add_organization_team_member, crate::backend::db::sqlite::organizations::teams::add_organization_team_member,
    fn add_organization_team_member(pool: &DbPool, actor: i64, org: i64, team: i64, target: i64, role: TeamRole) -> Result<OrganizationOutcome<TeamMembership>>);
native_operation!(crate::backend::db::postgres::organizations::teams::remove_organization_team_member, crate::backend::db::sqlite::organizations::teams::remove_organization_team_member,
    fn remove_organization_team_member(pool: &DbPool, actor: i64, org: i64, team: i64, target: i64) -> Result<OrganizationOutcome<()>>);
native_operation!(crate::backend::db::postgres::organizations::teams::grant_organization_team_repository, crate::backend::db::sqlite::organizations::teams::grant_organization_team_repository,
    fn grant_organization_team_repository(pool: &DbPool, actor: i64, org: i64, team: i64, repo: i64, permission: TeamRepositoryPermission) -> Result<OrganizationOutcome<TeamRepoAccess>>);
native_operation!(crate::backend::db::postgres::organizations::teams::revoke_organization_team_repository, crate::backend::db::sqlite::organizations::teams::revoke_organization_team_repository,
    fn revoke_organization_team_repository(pool: &DbPool, actor: i64, org: i64, team: i64, repo: i64) -> Result<OrganizationOutcome<()>>);
native_operation!(crate::backend::db::postgres::organizations::teams::list_organization_team_repositories, crate::backend::db::sqlite::organizations::teams::list_organization_team_repositories,
    fn list_organization_team_repositories(pool: &DbPool, actor: i64, org: i64, team: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<TeamRepositoryGrant>>>);
// Team pages are for active organization members only. Each read answers `None` when the viewer
// is not such a member, or the team is missing or hidden from the viewer.
native_operation!(crate::backend::db::postgres::organizations::teams::get_public_team, crate::backend::db::sqlite::organizations::teams::get_public_team,
    fn get_public_team(pool: &DbPool, actor: i64, org: i64, slug: &str) -> Result<Option<PublicTeam>>);
native_operation!(crate::backend::db::postgres::organizations::teams::list_public_team_members, crate::backend::db::sqlite::organizations::teams::list_public_team_members,
    fn list_public_team_members(pool: &DbPool, actor: i64, org: i64, team: i64, page: OrganizationPage) -> Result<Option<Vec<TeamMembership>>>);
native_operation!(crate::backend::db::postgres::organizations::teams::list_public_team_repositories, crate::backend::db::sqlite::organizations::teams::list_public_team_repositories,
    fn list_public_team_repositories(pool: &DbPool, actor: i64, org: i64, team: i64, page: OrganizationPage) -> Result<Option<Vec<TeamRepositoryGrant>>>);

#[derive(Debug, sqlx::FromRow)]
pub struct TeamRepositoryGrant {
    pub id: i64,
    pub repo_id: i64,
    pub owner: String,
    pub name: String,
    pub permission: i16,
}

/// A team row with its active member count, for the organization's team lists.
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct TeamListing {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub team: Team,
    pub member_count: i64,
}

/// A team as the public page shows it. `manage` is true for organization owners and admins
/// and for the team's maintainers; the management page checks again before any change.
#[derive(Debug)]
pub struct PublicTeam {
    pub team: Team,
    pub member_count: i64,
    pub repository_count: i64,
    pub manage: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamPrivacy {
    Visible,
    Secret,
}
impl TeamPrivacy {
    pub fn parse(value: &str) -> std::result::Result<Self, OrganizationInputError> {
        match value {
            "visible" => Ok(Self::Visible),
            "secret" => Ok(Self::Secret),
            _ => Err(OrganizationInputError::Privacy),
        }
    }
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Secret => "secret",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamRole {
    Member,
    Maintainer,
}
impl TeamRole {
    pub fn parse(value: &str) -> std::result::Result<Self, OrganizationInputError> {
        match value {
            "member" => Ok(Self::Member),
            "maintainer" => Ok(Self::Maintainer),
            _ => Err(OrganizationInputError::Role),
        }
    }
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Maintainer => "maintainer",
        }
    }
}
#[derive(Debug, Clone)]
pub struct TeamProfile {
    pub(in crate::backend::db) name: String,
    pub(in crate::backend::db) description: Option<String>,
    pub(in crate::backend::db) privacy: TeamPrivacy,
}
impl TeamProfile {
    pub fn new(
        name: &str,
        description: Option<&str>,
        privacy: TeamPrivacy,
    ) -> std::result::Result<Self, OrganizationInputError> {
        validate_name(name)?;
        validate_description(description)?;
        Ok(Self {
            name: name.to_owned(),
            description: description.map(str::to_owned),
            privacy,
        })
    }
}
#[derive(Debug, Clone)]
pub struct NewTeam {
    pub(in crate::backend::db) slug: String,
    pub(in crate::backend::db) profile: TeamProfile,
}
impl NewTeam {
    pub fn new(
        slug: &str,
        profile: TeamProfile,
    ) -> std::result::Result<Self, OrganizationInputError> {
        validate_slug(slug)?;
        Ok(Self {
            slug: slug.to_owned(),
            profile,
        })
    }
}
#[derive(Debug, Clone, Copy)]
pub struct TeamRepositoryPermission(pub(in crate::backend::db) i16);
impl TeamRepositoryPermission {
    pub fn new(value: i16) -> std::result::Result<Self, OrganizationInputError> {
        if !(1..=5).contains(&value) {
            return Err(OrganizationInputError::Permission);
        }
        Ok(Self(value))
    }
}
