mod exports;
use super::super::repositories::access::USER_REPO_ACCESS_SQL;
use super::invitations::RECIPIENT;
use crate::backend::db::{
    models::{OrgRole, Team, TeamMembership, TeamRepoAccess},
    organizations::*,
};
use anyhow::Result;
pub use exports::{
    list_direct_organization_team_members, list_direct_organization_team_repositories,
};
use sqlx::SqlitePool;
use OrganizationOutcome::{Applied, Conflict, Denied, Missing};

use super::authority::*;

/// One page of a team's members who are still active members of its organization.
const TEAM_MEMBERS_PAGE: &str = "SELECT tm.* FROM effective_team_memberships tm JOIN org_memberships om ON om.user_id=tm.user_id AND om.org_id=?1 JOIN users u ON u.id=tm.user_id WHERE tm.team_id=?2 AND tm.id>?3 AND tm.role IN ('member','maintainer') AND om.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password ORDER BY tm.id LIMIT ?4";

/// Team members with the role the GitHub API reports: organization owners count as maintainers.
const REPORTED_MEMBERS: &str = "SELECT tm.id, tm.team_id, tm.user_id, CASE WHEN om.role='owner' THEN 'maintainer' ELSE tm.role END AS role, tm.created_at FROM effective_team_memberships tm JOIN org_memberships om ON om.user_id=tm.user_id AND om.org_id=?1 JOIN users u ON u.id=tm.user_id WHERE tm.team_id=?2 AND tm.role IN ('member','maintainer') AND om.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password";

/// A team's visible parent in the same organization. Secret teams have no parent.
const PARENT: &str = "SELECT p.* FROM teams t JOIN teams p ON p.id=t.parent_team_id AND p.org_id=t.org_id WHERE t.id=?1 AND t.privacy='visible' AND p.privacy='visible'";

/// A team and every team below it.
const SUBTREE: &str = "SELECT team_id FROM team_ancestors WHERE ancestor_id=?1";

/// A team's repository grants, `?2` and `?4` being the caller's row filter and limit. Team
/// visibility is not READ, so the rows are filtered with same-organization team access plus
/// direct or public READ before the limit applies.
fn team_repositories_sql(filter: &str) -> String {
    format!("SELECT a.id, a.repo_id, a.permission, r.name, o.name AS owner FROM effective_team_repo_access a JOIN repositories r ON r.id=a.repo_id JOIN organizations o ON o.id=r.owner_org_id JOIN users u ON u.id=?6 WHERE a.team_id=?1 AND {filter} AND a.permission BETWEEN 1 AND 5 AND r.owner_kind='org' AND r.owner_org_id=?3 AND r.owner_user_id IS NULL AND (?5 OR NOT r.is_private OR ({USER_REPO_ACCESS_SQL})>=1) ORDER BY a.id LIMIT ?4")
}

/// The team when the actor may read it: an active member of its organization who can see it.
async fn readable_team(
    connection: &mut sqlx::SqliteConnection,
    actor: i64,
    org: i64,
    team: i64,
) -> Result<Option<(OrgRole, Team)>> {
    let Applied(role) = read_authority(connection, actor, org).await? else {
        return Ok(None);
    };
    let Some(team) = scoped_team(connection, org, team).await? else {
        return Ok(None);
    };
    Ok(team_visible(connection, actor, role, &team)
        .await?
        .then_some((role, team)))
}

/// The team page's counts and parent, read in the transaction that authorized the team.
async fn public_team(
    connection: &mut sqlx::SqliteConnection,
    actor: i64,
    role: OrgRole,
    team: Team,
) -> Result<PublicTeam> {
    let org = team.org_id;
    let member_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM effective_team_memberships tm JOIN org_memberships om ON om.user_id=tm.user_id AND om.org_id=?1 JOIN users u ON u.id=tm.user_id WHERE tm.team_id=?2 AND tm.role IN ('member','maintainer') AND om.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(org).bind(team.id).fetch_one(&mut *connection).await?;
    let repository_count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM effective_team_repo_access a JOIN repositories r ON r.id=a.repo_id JOIN users u ON u.id=?4 WHERE a.team_id=?1 AND a.permission BETWEEN 1 AND 5 AND r.owner_kind='org' AND r.owner_org_id=?2 AND r.owner_user_id IS NULL AND (?3 OR NOT r.is_private OR ({USER_REPO_ACCESS_SQL})>=1)"))
        .bind(team.id).bind(org).bind(administers(role)).bind(actor).fetch_one(&mut *connection).await?;
    let parent = sqlx::query_as(PARENT)
        .bind(team.id)
        .fetch_optional(connection)
        .await?;
    Ok(PublicTeam {
        manage: administers(role),
        team,
        member_count,
        repository_count,
        parent,
    })
}

async fn manages_team(c: &mut sqlx::SqliteConnection, actor: i64, org: i64, team: i64, role: OrgRole) -> Result<bool> {
    if administers(role) { return Ok(true); }
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM teams t JOIN team_memberships m ON m.team_id=t.id WHERE t.id=?1 AND t.org_id=?2 AND m.user_id=?3 AND m.role='maintainer')")
        .bind(team).bind(org).bind(actor).fetch_one(c).await?)
}

pub async fn create_organization_team(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    input: &NewTeam,
) -> Result<OrganizationOutcome<Team>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let role = require_org!(tx, actor, org);
    if !administers(role) {
        return Ok(Denied);
    }
    if input.maintainers.len() > 100 || input.repositories.len() > 100 {
        return Ok(Conflict);
    }
    for &user in &input.maintainers {
        if !active(&mut tx, user).await?
            || membership_role(&mut tx, org, user)
                .await?
                .and_then(|role| role.parse::<OrgRole>().ok())
                .is_none()
        {
            return Ok(Conflict);
        }
    }
    for &repo in &input.repositories {
        if !scoped_repo(&mut tx, org, repo).await? {
            return Ok(Conflict);
        }
    }
    let parent = input.profile.parent_team_id.flatten();
    validate_parent(&mut tx, org, 0, parent, input.privacy).await?;
    let team: Option<Team> = sqlx::query_as("INSERT INTO teams(org_id,slug,name,description,privacy,default_permission,notifications_enabled,parent_team_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(org_id,slug) DO NOTHING RETURNING *")
        .bind(org).bind(&input.slug).bind(&input.name).bind(input.profile.description.clone().flatten()).bind(input.privacy.as_str()).bind(input.profile.default_permission.unwrap_or(1)).bind(input.profile.notifications_enabled.unwrap_or(true)).bind(parent).fetch_optional(&mut *tx).await?;
    let Some(team) = team else {
        return Ok(Conflict);
    };
    for &user in &input.maintainers {
        sqlx::query("INSERT INTO team_memberships(team_id,user_id,role) VALUES(?1,?2,'maintainer') ON CONFLICT(team_id,user_id) DO NOTHING")
            .bind(team.id).bind(user).execute(&mut *tx).await?;
    }
    for &repo in &input.repositories {
        sqlx::query("INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES(?1,?2,?3) ON CONFLICT(team_id,repo_id) DO NOTHING")
            .bind(team.id).bind(repo).bind(team.default_permission).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Applied(team))
}
pub async fn update_organization_team(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    input: &TeamProfile,
) -> Result<OrganizationOutcome<Team>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let role = require_org!(tx, actor, org);
    if !manages_team(&mut tx, actor, org, team, role).await? { return Ok(Denied); }
    let Some(current) = scoped_team(&mut tx, org, team).await? else {
        return Ok(Missing);
    };
    let parent = input.parent_team_id.unwrap_or(current.parent_team_id);
    if !administers(role) && parent != current.parent_team_id {
        if let Some(parent) = parent {
            if !manages_team(&mut tx, actor, org, parent, role).await? { return Ok(Denied); }
        }
    }
    let privacy = match input.privacy {
        Some(privacy) => privacy,
        None => TeamPrivacy::parse(&current.privacy)
            .map_err(|_| anyhow::anyhow!("Invalid stored team privacy"))?,
    };
    validate_parent(&mut tx, org, team, parent, privacy).await?;
    if let Some(slug) = &input.slug {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM teams WHERE org_id=?1 AND slug=?2 AND id<>?3)",
        )
        .bind(org)
        .bind(slug)
        .bind(team)
        .fetch_one(&mut *tx)
        .await?;
        if exists {
            return Ok(Conflict);
        }
    }
    // Only losing an inherited parent can revoke access. The move stops the runs of this team's
    // members that it takes below write access; runs already below it are left alone.
    let team_id = team;
    let reparented = parent != current.parent_team_id && current.parent_team_id.is_some();
    let denied_runs = format!("SELECT c.id FROM ci_runs c JOIN repositories r ON r.id=c.repo_id JOIN users u ON u.id=c.triggered_by_id WHERE r.owner_kind='org' AND r.owner_org_id=?1 AND c.triggered_by_id IN (SELECT user_id FROM effective_team_memberships WHERE team_id=?2) AND (c.status IN ('queued','running') OR c.completed_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour')) AND ({USER_REPO_ACCESS_SQL}) < CASE WHEN c.pipeline IN ('cd','full') OR c.environment IS NOT NULL THEN 5 ELSE 3 END");
    let denied_before: Vec<i64> = if reparented {
        sqlx::query_scalar(&denied_runs)
            .bind(org)
            .bind(team_id)
            .fetch_all(&mut *tx)
            .await?
    } else {
        Vec::new()
    };
    let team = sqlx::query_as("UPDATE teams SET name=COALESCE(?3,name),description=CASE WHEN ?10 THEN ?4 ELSE description END,privacy=?5,slug=COALESCE(?6,slug),default_permission=COALESCE(?7,default_permission),notifications_enabled=COALESCE(?8,notifications_enabled),parent_team_id=?9,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 AND org_id=?2 RETURNING *")
        .bind(team).bind(org).bind(&input.name).bind(input.description.clone().flatten()).bind(privacy.as_str()).bind(&input.slug).bind(input.default_permission).bind(input.notifications_enabled).bind(parent).bind(input.description.is_some()).fetch_one(&mut *tx).await?;
    if reparented {
        let denied: Vec<i64> = sqlx::query_scalar(&denied_runs)
            .bind(org)
            .bind(team_id)
            .fetch_all(&mut *tx)
            .await?;
        let runs: Vec<i64> = denied
            .into_iter()
            .filter(|run| !denied_before.contains(run))
            .collect();
        super::super::ci::stop_ci_runs(&mut tx, &runs).await?;
    }
    tx.commit().await?;
    Ok(Applied(team))
}
pub async fn delete_organization_team(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let role = require_org!(tx, actor, org);
    if !manages_team(&mut tx, actor, org, team, role).await? { return Ok(Denied); }
    if scoped_team(&mut tx, org, team).await?.is_none() {
        return Ok(Missing);
    }
    // The runs of everyone on the team or a child team, on repositories that any of them reach.
    let runs: Vec<i64> = sqlx::query_scalar(&format!("SELECT id FROM ci_runs WHERE (status IN ('queued','running') OR completed_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour')) AND triggered_by_id IN (SELECT user_id FROM effective_team_memberships WHERE team_id IN ({SUBTREE})) AND repo_id IN (SELECT r.id FROM repositories r WHERE r.owner_kind='org' AND r.owner_org_id=?2 AND r.owner_user_id IS NULL AND (EXISTS(SELECT 1 FROM effective_team_repo_access a WHERE a.team_id IN ({SUBTREE}) AND a.repo_id=r.id AND a.permission>=3) OR EXISTS(SELECT 1 FROM organization_role_assignments a JOIN organization_roles role ON role.id=a.role_id AND role.base_level BETWEEN 3 AND 5 JOIN team_ancestors lineage ON lineage.ancestor_id=a.team_id WHERE lineage.team_id IN ({SUBTREE}) AND a.org_id=?2 AND a.user_id IS NULL)))"))
        .bind(team).bind(org).fetch_all(&mut *tx).await?;
    super::super::ci::stop_ci_runs(&mut tx, &runs).await?;
    let teams: Vec<i64> = sqlx::query_scalar(SUBTREE)
        .bind(team)
        .fetch_all(&mut *tx)
        .await?;
    let teams = serde_json::to_string(&teams)?;
    sqlx::query("DELETE FROM team_repo_access WHERE team_id IN (SELECT value FROM json_each(?1))")
        .bind(&teams)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM team_memberships WHERE team_id IN (SELECT value FROM json_each(?1))")
        .bind(&teams)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM teams WHERE org_id=?2 AND id IN (SELECT value FROM json_each(?1))")
        .bind(&teams)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(()))
}
pub async fn list_organization_teams(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<TeamListing>>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    let mut teams: Vec<TeamListing> = sqlx::query_as("SELECT t.*, (SELECT COUNT(*) FROM effective_team_memberships tm JOIN org_memberships om ON om.user_id=tm.user_id AND om.org_id=t.org_id JOIN users u ON u.id=tm.user_id WHERE tm.team_id=t.id AND tm.role IN ('member','maintainer') AND om.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password) AS member_count FROM teams t WHERE t.org_id=?1 AND t.id>?2 AND (t.privacy='visible' OR (t.privacy='secret' AND (?3 OR EXISTS(SELECT 1 FROM team_memberships m WHERE m.team_id=t.id AND m.user_id=?4 AND m.role IN ('member','maintainer'))))) ORDER BY t.id LIMIT ?5")
        .bind(org).bind(page.after).bind(administers(role)).bind(actor).bind(page.limit).fetch_all(&mut *tx).await?;
    let ids: Vec<i64> = teams
        .iter()
        .filter(|row| row.team.privacy == "visible")
        .filter_map(|row| row.team.parent_team_id)
        .collect();
    let parents: Vec<Team> = sqlx::query_as("SELECT * FROM teams WHERE org_id=?1 AND privacy='visible' AND id IN (SELECT value FROM json_each(?2))")
        .bind(org).bind(serde_json::to_string(&ids)?).fetch_all(&mut *tx).await?;
    for row in &mut teams {
        row.parent = row
            .team
            .parent_team_id
            .filter(|_| row.team.privacy == "visible")
            .and_then(|id| parents.iter().find(|parent| parent.id == id).cloned());
    }
    tx.commit().await?;
    Ok(Applied(teams))
}
pub async fn count_organization_teams(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<i64>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    let count = sqlx::query_scalar("SELECT COUNT(*) FROM teams t WHERE t.org_id=?1 AND (t.privacy='visible' OR (t.privacy='secret' AND (?2 OR EXISTS(SELECT 1 FROM team_memberships m WHERE m.team_id=t.id AND m.user_id=?3 AND m.role IN ('member','maintainer')))))")
        .bind(org).bind(administers(role)).bind(actor)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(count))
}
pub async fn list_organization_team_members(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<TeamMembership>>> {
    list_members(pool, actor, org, team, page, false).await
}
async fn list_members(pool: &SqlitePool, actor: i64, org: i64, team: i64, page: OrganizationPage, direct: bool) -> Result<OrganizationOutcome<Vec<TeamMembership>>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    let Some(team) = scoped_team(&mut tx, org, team).await? else {
        return Ok(Missing);
    };
    if !team_visible(&mut tx, actor, role, &team).await? {
        return Ok(Denied);
    }
    let sql = if direct { TEAM_MEMBERS_PAGE.replace("effective_team_memberships", "team_memberships") } else { TEAM_MEMBERS_PAGE.to_owned() };
    let members = sqlx::query_as(&sql)
        .bind(org).bind(team.id).bind(page.after).bind(page.limit)
        .fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(members))
}
pub async fn add_organization_team_member(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    target: i64,
    role: TeamRole,
) -> Result<OrganizationOutcome<TeamMembership>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let actor_role = require_org!(tx, actor, org);
    if !manages_team(&mut tx, actor, org, team, actor_role).await? { return Ok(Denied); }
    if !active(&mut tx, target).await? {
        return Ok(Denied);
    }
    if membership_role(&mut tx, org, target)
        .await?
        .and_then(|s| s.parse::<OrgRole>().ok())
        .is_none()
    {
        return Ok(Denied);
    }
    if scoped_team(&mut tx, org, team).await?.is_none() {
        return Ok(Missing);
    }
    let member = sqlx::query_as("INSERT INTO team_memberships(team_id,user_id,role) VALUES(?1,?2,?3) ON CONFLICT(team_id,user_id) DO UPDATE SET role=EXCLUDED.role RETURNING *")
        .bind(team).bind(target).bind(role.as_str()).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(member))
}
pub async fn remove_organization_team_member(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    target: i64,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let role = require_org!(tx, actor, org);
    if !manages_team(&mut tx, actor, org, team, role).await? { return Ok(Denied); }
    // Exact scoped cleanup, not a grant: target activity/org role is irrelevant.
    if scoped_team(&mut tx, org, team).await?.is_none() {
        return Ok(Missing);
    }
    let recipient = RECIPIENT.replace("?1", "?3");
    let pending = sqlx::query(&format!("DELETE FROM team_invitations WHERE team_id=?1 AND invitation_id IN (SELECT i.id FROM organization_invitations i WHERE i.org_id=?2 AND {recipient})"))
        .bind(team).bind(org).bind(target).execute(&mut *tx).await?.rows_affected();
    if sqlx::query("DELETE FROM team_memberships WHERE team_id=?1 AND user_id=?2")
        .bind(team)
        .bind(target)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
        && pending == 0
    {
        let inherited: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM effective_team_memberships WHERE team_id=?1 AND user_id=?2)").bind(team).bind(target).fetch_one(&mut *tx).await?;
        return Ok(if inherited { Denied } else { Missing });
    }
    // Only repositories reached through this team's grants or inherited organization roles.
    let runs:Vec<i64>=sqlx::query_scalar("SELECT id FROM ci_runs WHERE triggered_by_id=?1 AND (status IN ('queued','running') OR completed_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour')) AND repo_id IN (SELECT r.id FROM repositories r WHERE r.owner_kind='org' AND r.owner_org_id=?3 AND r.owner_user_id IS NULL AND (EXISTS(SELECT 1 FROM effective_team_repo_access a WHERE a.team_id=?2 AND a.repo_id=r.id AND a.permission>=3) OR EXISTS(SELECT 1 FROM organization_role_assignments a JOIN organization_roles role ON role.id=a.role_id AND role.base_level BETWEEN 3 AND 5 JOIN team_ancestors lineage ON lineage.ancestor_id=a.team_id WHERE lineage.team_id=?2 AND a.org_id=?3 AND a.user_id IS NULL)))").bind(target).bind(team).bind(org).fetch_all(&mut *tx).await?;
    super::super::ci::stop_ci_runs(&mut tx, &runs).await?;
    tx.commit().await?;
    Ok(Applied(()))
}
pub async fn grant_organization_team_repository(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    repo: i64,
    permission: TeamRepositoryPermission,
) -> Result<OrganizationOutcome<TeamRepoAccess>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let role = require_org!(tx, actor, org);
    if !administers(role) {
        return Ok(Denied);
    }
    if scoped_team(&mut tx, org, team).await?.is_none() {
        return Ok(Missing);
    }
    if !scoped_repo(&mut tx, org, repo).await? {
        return Ok(Denied);
    }
    let old: Option<i16> = sqlx::query_scalar(
        "SELECT permission FROM team_repo_access WHERE team_id=?1 AND repo_id=?2",
    )
    .bind(team)
    .bind(repo)
    .fetch_optional(&mut *tx)
    .await?;
    if old.is_some_and(|level| level > permission.0 || !(1..=5).contains(&level)) {
        cancel_team(&mut tx, org, team, repo).await?;
    }
    let access = sqlx::query_as("INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES(?1,?2,?3) ON CONFLICT(team_id,repo_id) DO UPDATE SET permission=EXCLUDED.permission RETURNING *")
        .bind(team).bind(repo).bind(permission.0).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(access))
}
pub async fn revoke_organization_team_repository(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    repo: i64,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let role = require_org!(tx, actor, org);
    if !administers(role) {
        return Ok(Denied);
    }
    if scoped_team(&mut tx, org, team).await?.is_none() {
        return Ok(Missing);
    }
    // Own-tenant ownership (even stale lifecycle) permits scoped cancellation;
    // foreign repos are never changed/cancelled by this repair.
    let own_repo = sqlx::query_scalar::<_, i64>("SELECT id FROM repositories WHERE id=?1 AND owner_kind='org' AND owner_org_id=?2 AND owner_user_id IS NULL")
        .bind(repo).bind(org).fetch_optional(&mut *tx).await?.is_some();
    if sqlx::query("DELETE FROM team_repo_access WHERE team_id=?1 AND repo_id=?2")
        .bind(team)
        .bind(repo)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
    {
        let inherited: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM effective_team_repo_access WHERE team_id=?1 AND repo_id=?2)").bind(team).bind(repo).fetch_one(&mut *tx).await?;
        return Ok(if inherited { Denied } else { Missing });
    }
    if own_repo {
        cancel_team(&mut tx, org, team, repo).await?;
    }
    tx.commit().await?;
    Ok(Applied(()))
}
pub async fn list_organization_team_repositories(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<TeamRepositoryGrant>>> {
    list_repositories(pool, actor, org, team, page, false).await
}
async fn list_repositories(pool: &SqlitePool, actor: i64, org: i64, team: i64, page: OrganizationPage, direct: bool) -> Result<OrganizationOutcome<Vec<TeamRepositoryGrant>>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    let Some(team) = scoped_team(&mut tx, org, team).await? else {
        return Ok(Missing);
    };
    if !team_visible(&mut tx, actor, role, &team).await? {
        return Ok(Denied);
    }
    let sql = team_repositories_sql("a.id>?2");
    let sql = if direct { sql.replace("effective_team_repo_access", "team_repo_access") } else { sql };
    let rows = sqlx::query_as(&sql)
        .bind(team.id).bind(page.after).bind(org).bind(page.limit).bind(administers(role)).bind(actor)
        .fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
// Team pages are for active organization members only: anyone else gets the answer of a missing team.
pub async fn get_public_team(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    slug: &str,
) -> Result<Option<PublicTeam>> {
    let mut tx = begin_read(pool).await?;
    let Applied(role) = read_authority(&mut tx, actor, org).await? else {
        return Ok(None);
    };
    let Some(team) = sqlx::query_as::<_, Team>("SELECT * FROM teams WHERE org_id=?1 AND slug=?2")
        .bind(org)
        .bind(slug)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(None);
    };
    if !team_visible(&mut tx, actor, role, &team).await? {
        return Ok(None);
    }
    let public = public_team(&mut tx, actor, role, team).await?;
    tx.commit().await?;
    Ok(Some(public))
}
/// The same read by team ID, for the routes that name no organization.
pub async fn get_public_team_by_id(
    pool: &SqlitePool,
    actor: i64,
    team: i64,
) -> Result<Option<PublicTeam>> {
    let mut tx = begin_read(pool).await?;
    let Some(org) = sqlx::query_scalar::<_, i64>("SELECT org_id FROM teams WHERE id=?1")
        .bind(team)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(None);
    };
    let Some((role, team)) = readable_team(&mut tx, actor, org, team).await? else {
        return Ok(None);
    };
    let public = public_team(&mut tx, actor, role, team).await?;
    tx.commit().await?;
    Ok(Some(public))
}
pub async fn list_public_team_members(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<Option<Vec<TeamMembership>>> {
    Ok(match list_members(pool, actor, org, team, page, false).await? {
        Applied(rows) => Some(rows),
        _ => None,
    })
}
pub async fn list_public_team_repositories(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<Option<Vec<TeamRepositoryGrant>>> {
    Ok(match list_repositories(pool, actor, org, team, page, false).await? {
        Applied(rows) => Some(rows),
        _ => None,
    })
}
pub async fn get_public_team_repository(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    repo: i64,
) -> Result<Option<Option<TeamRepositoryGrant>>> {
    let mut tx = begin_read(pool).await?;
    let Some((role, team)) = readable_team(&mut tx, actor, org, team).await? else {
        return Ok(None);
    };
    let grant = sqlx::query_as(&team_repositories_sql("a.repo_id=?2"))
        .bind(team.id)
        .bind(repo)
        .bind(org)
        .bind(1_i64)
        .bind(administers(role))
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(grant))
}
pub async fn list_team_members_by_role(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    role: Option<TeamRole>,
    page: OrganizationPage,
) -> Result<Option<Vec<TeamMembership>>> {
    let mut tx = begin_read(pool).await?;
    let Some((_, team)) = readable_team(&mut tx, actor, org, team).await? else {
        return Ok(None);
    };
    let members = sqlx::query_as(&format!("{REPORTED_MEMBERS} AND tm.id>?3 AND (?5 IS NULL OR (CASE WHEN om.role='owner' THEN 'maintainer' ELSE tm.role END)=?5) ORDER BY tm.id LIMIT ?4"))
        .bind(org)
        .bind(team.id)
        .bind(page.after)
        .bind(page.limit)
        .bind(role.map(TeamRole::as_str))
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(members))
}
pub async fn get_team_member(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    user: i64,
) -> Result<Option<Option<TeamMembership>>> {
    let mut tx = begin_read(pool).await?;
    let Some((_, team)) = readable_team(&mut tx, actor, org, team).await? else {
        return Ok(None);
    };
    let member = sqlx::query_as(&format!("{REPORTED_MEMBERS} AND tm.user_id=?3"))
        .bind(org)
        .bind(team.id)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(member))
}

/// Users on a team who are current members of its organization. Code owner names such as
/// `@org/team` resolve through this.
pub async fn team_members_by_names(
    pool: &SqlitePool,
    org_id: i64,
    teams: &[String],
) -> Result<Vec<(String, i64)>> {
    anyhow::ensure!(teams.len() <= 500, "Too many teams");
    Ok(sqlx::query_as("SELECT o.name || '/' || t.slug, tm.user_id FROM effective_team_memberships tm JOIN teams t ON t.id=tm.team_id JOIN organizations o ON o.id=t.org_id JOIN org_memberships om ON om.user_id=tm.user_id AND om.org_id=o.id JOIN users u ON u.id=tm.user_id WHERE o.name || '/' || t.slug IN (SELECT value FROM json_each(?1)) AND o.id=?2 AND t.privacy='visible' AND tm.role IN ('member','maintainer') AND om.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password ORDER BY tm.user_id LIMIT 5001")
        .bind(serde_json::to_string(teams)?)
        .bind(org_id)
        .fetch_all(pool)
        .await?)
}

async fn validate_parent(
    connection: &mut sqlx::SqliteConnection,
    org: i64,
    team: i64,
    parent: Option<i64>,
    privacy: TeamPrivacy,
) -> Result<()> {
    if privacy == TeamPrivacy::Secret {
        let children: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM teams WHERE parent_team_id=?1)")
                .bind(team)
                .fetch_one(&mut *connection)
                .await?;
        anyhow::ensure!(parent.is_none() && !children, InvalidTeamParent);
    }
    if let Some(parent) = parent {
        let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM teams p WHERE p.id=?1 AND p.org_id=?2 AND p.privacy='visible' AND NOT EXISTS(SELECT 1 FROM team_ancestors a WHERE a.team_id=p.id AND a.ancestor_id=?3))")
            .bind(parent).bind(org).bind(team).fetch_one(connection).await?;
        anyhow::ensure!(valid, InvalidTeamParent);
    }
    Ok(())
}

pub async fn list_child_teams(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<Option<Vec<Team>>> {
    let mut tx = begin_read(pool).await?;
    if readable_team(&mut tx, actor, org, team).await?.is_none() {
        return Ok(None);
    }
    let rows = sqlx::query_as("SELECT * FROM teams WHERE org_id=?1 AND parent_team_id=?2 AND privacy='visible' AND id>?3 ORDER BY id LIMIT ?4")
        .bind(org).bind(team).bind(page.after).bind(page.limit).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(rows))
}

pub async fn get_team_parent(pool: &SqlitePool, team: i64) -> Result<Option<Team>> {
    Ok(sqlx::query_as(PARENT)
        .bind(team)
        .fetch_optional(pool)
        .await?)
}

pub async fn get_team_by_id(pool: &SqlitePool, actor: i64, team: i64) -> Result<Option<Team>> {
    let mut tx = begin_read(pool).await?;
    let Some(team) = sqlx::query_as::<_, Team>("SELECT * FROM teams WHERE id=?1")
        .bind(team)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(None);
    };
    let Applied(role) = read_authority(&mut tx, actor, team.org_id).await? else {
        return Ok(None);
    };
    if !team_visible(&mut tx, actor, role, &team).await? {
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(team))
}

pub async fn list_user_teams(
    pool: &SqlitePool,
    actor: i64,
    offset: i64,
    limit: i64,
) -> Result<Vec<Team>> {
    anyhow::ensure!(
        (0..=1_000_000).contains(&offset) && (1..=101).contains(&limit),
        "Invalid page"
    );
    Ok(sqlx::query_as("SELECT t.* FROM teams t JOIN effective_team_memberships tm ON tm.team_id=t.id JOIN org_memberships om ON om.org_id=t.org_id AND om.user_id=tm.user_id JOIN users u ON u.id=tm.user_id WHERE u.id=?1 AND NOT u.is_banned AND NOT u.must_change_password AND om.role IN ('owner','admin','member') AND tm.role IN ('member','maintainer') AND t.privacy IN ('visible','secret') AND NOT EXISTS(SELECT 1 FROM organization_blocks b WHERE b.org_id=t.org_id AND b.user_id=u.id) AND EXISTS(SELECT 1 FROM org_memberships own JOIN users usable ON usable.id=own.user_id WHERE own.org_id=t.org_id AND own.role='owner' AND NOT usable.is_banned AND NOT usable.must_change_password) ORDER BY t.id LIMIT ?2 OFFSET ?3")
        .bind(actor).bind(limit).bind(offset).fetch_all(pool).await?)
}
