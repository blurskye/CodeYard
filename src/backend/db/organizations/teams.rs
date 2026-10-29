use super::profiles::{validate_description, validate_name, validate_slug};
use super::*;
use crate::backend::db::{
    models::{Team, TeamMembership, TeamRepoAccess},
    DbPool,
};
use anyhow::Result;

/// A lean identity read: credential, organization policy and team visibility share one snapshot.
pub enum TeamIdentity<'a> {
    Id(i64),
    Slug(i64, &'a str),
}
native_operation!(crate::backend::db::postgres::organizations::teams::team_identity,
    crate::backend::db::sqlite::organizations::teams::team_identity,
    fn team_identity(pool: &DbPool, actor: i64, token: i64, identity: TeamIdentity<'_>) -> Result<Option<Team>>);

native_operation!(crate::backend::db::postgres::organizations::teams::create_organization_team, crate::backend::db::sqlite::organizations::teams::create_organization_team,
    fn create_organization_team(pool: &DbPool, actor: i64, org: i64, input: &NewTeam) -> Result<OrganizationOutcome<Team>>);
native_operation!(crate::backend::db::postgres::organizations::teams::update_organization_team, crate::backend::db::sqlite::organizations::teams::update_organization_team,
    fn update_organization_team(pool: &DbPool, actor: i64, org: i64, team: i64, input: &TeamProfile) -> Result<OrganizationOutcome<Team>>);
// Deleting a team deletes its child teams with it, as on GitHub.
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
// Migration archives retain direct edges and team parents, not flattened inherited access.
native_operation!(crate::backend::db::postgres::organizations::teams::list_direct_organization_team_members, crate::backend::db::sqlite::organizations::teams::list_direct_organization_team_members,
    fn list_direct_organization_team_members(pool: &DbPool, actor: i64, org: i64, team: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<TeamMembership>>>);
native_operation!(crate::backend::db::postgres::organizations::teams::list_direct_organization_team_repositories, crate::backend::db::sqlite::organizations::teams::list_direct_organization_team_repositories,
    fn list_direct_organization_team_repositories(pool: &DbPool, actor: i64, org: i64, team: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<TeamRepositoryGrant>>>);
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
native_operation!(crate::backend::db::postgres::organizations::teams::get_public_team_by_id, crate::backend::db::sqlite::organizations::teams::get_public_team_by_id,
    fn get_public_team_by_id(pool: &DbPool, actor: i64, team: i64) -> Result<Option<PublicTeam>>);
native_operation!(crate::backend::db::postgres::organizations::teams::list_public_team_members, crate::backend::db::sqlite::organizations::teams::list_public_team_members,
    fn list_public_team_members(pool: &DbPool, actor: i64, org: i64, team: i64, page: OrganizationPage) -> Result<Option<Vec<TeamMembership>>>);
native_operation!(crate::backend::db::postgres::organizations::teams::list_public_team_repositories, crate::backend::db::sqlite::organizations::teams::list_public_team_repositories,
    fn list_public_team_repositories(pool: &DbPool, actor: i64, org: i64, team: i64, page: OrganizationPage) -> Result<Option<Vec<TeamRepositoryGrant>>>);
// The GitHub API reports organization owners as maintainers of every team they are on, and
// filters by that role. The outer `None` means the team is not readable, as for the reads above.
native_operation!(crate::backend::db::postgres::organizations::teams::list_team_members_by_role, crate::backend::db::sqlite::organizations::teams::list_team_members_by_role,
    fn list_team_members_by_role(pool: &DbPool, actor: i64, org: i64, team: i64, role: Option<TeamRole>, page: OrganizationPage) -> Result<Option<Vec<TeamMembership>>>);
native_operation!(crate::backend::db::postgres::organizations::teams::get_team_member, crate::backend::db::sqlite::organizations::teams::get_team_member,
    fn get_team_member(pool: &DbPool, actor: i64, org: i64, team: i64, user: i64) -> Result<Option<Option<TeamMembership>>>);
native_operation!(crate::backend::db::postgres::organizations::teams::get_public_team_repository, crate::backend::db::sqlite::organizations::teams::get_public_team_repository,
    fn get_public_team_repository(pool: &DbPool, actor: i64, org: i64, team: i64, repo: i64) -> Result<Option<Option<TeamRepositoryGrant>>>);

#[derive(Debug, sqlx::FromRow)]
pub struct TeamRepositoryGrant {
    pub id: i64,
    pub repo_id: i64,
    pub owner: String,
    pub name: String,
    pub permission: i16,
}

/// A team row with its active member count and visible parent, for the organization's team
/// lists. The parent is read in the same transaction that authorized the list.
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct TeamListing {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub team: Team,
    pub member_count: i64,
    #[sqlx(skip)]
    #[serde(skip)]
    pub parent: Option<Team>,
}

/// A team as the public page shows it. `manage` is true for organization owners and admins;
/// the management page checks again before any change. The parent is read in the same
/// transaction that authorized the team.
#[derive(Debug)]
pub struct PublicTeam {
    pub team: Team,
    pub member_count: i64,
    pub repository_count: i64,
    pub manage: bool,
    pub parent: Option<Team>,
}

#[derive(Debug, thiserror::Error)]
#[error("Team parents must be visible teams in the same organization and cannot form a cycle")]
pub struct InvalidTeamParent;

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
/// A team's name, description and settings. A field left `None` keeps the stored value, so an
/// edit changes only the fields it names and never restores one read before a concurrent change.
#[derive(Debug, Clone, Default)]
pub struct TeamProfile {
    pub(in crate::backend::db) name: Option<String>,
    pub(in crate::backend::db) description: Option<Option<String>>,
    pub(in crate::backend::db) privacy: Option<TeamPrivacy>,
    pub(in crate::backend::db) slug: Option<String>,
    pub(in crate::backend::db) default_permission: Option<i16>,
    pub(in crate::backend::db) notifications_enabled: Option<bool>,
    pub(in crate::backend::db) parent_team_id: Option<Option<i64>>,
}
impl TeamProfile {
    /// A complete profile, as a form that shows every field submits it.
    pub fn new(
        name: &str,
        description: Option<&str>,
        privacy: TeamPrivacy,
    ) -> std::result::Result<Self, OrganizationInputError> {
        Ok(Self::default()
            .with_name(name)?
            .with_description(description)?
            .with_privacy(privacy))
    }

    pub fn with_name(mut self, name: &str) -> std::result::Result<Self, OrganizationInputError> {
        validate_name(name)?;
        self.name = Some(name.to_owned());
        Ok(self)
    }

    pub fn with_description(
        mut self,
        description: Option<&str>,
    ) -> std::result::Result<Self, OrganizationInputError> {
        validate_description(description)?;
        self.description = Some(description.map(str::to_owned));
        Ok(self)
    }

    pub fn with_privacy(mut self, privacy: TeamPrivacy) -> Self {
        self.privacy = Some(privacy);
        self
    }

    pub fn with_parent(
        mut self,
        parent: Option<Option<i64>>,
    ) -> std::result::Result<Self, OrganizationInputError> {
        if parent.flatten().is_some_and(|id| id <= 0) {
            return Err(OrganizationInputError::Parent);
        }
        self.parent_team_id = parent;
        Ok(self)
    }

    pub fn with_settings(
        mut self,
        slug: Option<&str>,
        permission: Option<TeamRepositoryPermission>,
        notifications_enabled: Option<bool>,
    ) -> std::result::Result<Self, OrganizationInputError> {
        if let Some(slug) = slug {
            validate_slug(slug)?;
        }
        self.slug = slug.map(str::to_owned);
        self.default_permission = permission.map(|permission| permission.0);
        self.notifications_enabled = notifications_enabled;
        Ok(self)
    }
}
/// A new team needs a name and a privacy; the rest of its profile is optional.
#[derive(Debug, Clone)]
pub struct NewTeam {
    pub(in crate::backend::db) slug: String,
    pub(in crate::backend::db) name: String,
    pub(in crate::backend::db) privacy: TeamPrivacy,
    pub(in crate::backend::db) profile: TeamProfile,
    pub(in crate::backend::db) maintainers: Vec<i64>,
    pub(in crate::backend::db) repositories: Vec<i64>,
}
impl NewTeam {
    pub fn new(
        slug: &str,
        profile: TeamProfile,
    ) -> std::result::Result<Self, OrganizationInputError> {
        validate_slug(slug)?;
        let name = profile.name.clone().ok_or(OrganizationInputError::Name)?;
        let privacy = profile.privacy.ok_or(OrganizationInputError::Privacy)?;
        Ok(Self {
            slug: slug.to_owned(),
            name,
            privacy,
            profile,
            maintainers: Vec::new(),
            repositories: Vec::new(),
        })
    }

    pub fn with_members_and_repositories(
        mut self,
        maintainers: Vec<i64>,
        repositories: Vec<i64>,
    ) -> Self {
        self.maintainers = maintainers;
        self.repositories = repositories;
        self
    }
}

// A team reference outside the team pages, such as a requested reviewer, expands its visible
// parent through this. Team pages read the parent with the team instead.
native_operation!(crate::backend::db::postgres::organizations::teams::get_team_parent, crate::backend::db::sqlite::organizations::teams::get_team_parent,
    fn get_team_parent(pool: &DbPool, team: i64) -> Result<Option<Team>>);
native_operation!(crate::backend::db::postgres::organizations::teams::get_team_by_id, crate::backend::db::sqlite::organizations::teams::get_team_by_id,
    fn get_team_by_id(pool: &DbPool, actor: i64, team: i64) -> Result<Option<Team>>);
native_operation!(crate::backend::db::postgres::organizations::teams::list_child_teams, crate::backend::db::sqlite::organizations::teams::list_child_teams,
    fn list_child_teams(pool: &DbPool, actor: i64, org: i64, team: i64, page: OrganizationPage) -> Result<Option<Vec<Team>>>);
native_operation!(crate::backend::db::postgres::organizations::teams::list_user_teams, crate::backend::db::sqlite::organizations::teams::list_user_teams,
    fn list_user_teams(pool: &DbPool, actor: i64, offset: i64, limit: i64) -> Result<Vec<Team>>);

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
