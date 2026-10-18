use crate::backend::db::{
    models::{OrgRole, Team, TeamMembership, TeamRepoAccess},
    organizations::*,
};
use anyhow::Result;
use sqlx::PgPool;
use OrganizationOutcome::{Applied, Conflict, Denied, Missing};

use super::authority::*;

pub async fn create_organization_team(
    pool: &PgPool,
    actor: i64,
    org: i64,
    input: &NewTeam,
) -> Result<OrganizationOutcome<Team>> {
    let mut tx = begin(pool).await?;
    let role = require_org!(tx, actor, org);
    if !administers(role) {
        return Ok(Denied);
    }
    let team = sqlx::query_as("INSERT INTO teams(org_id,slug,name,description,privacy) VALUES($1,$2,$3,$4,$5) ON CONFLICT(org_id,slug) DO NOTHING RETURNING *")
        .bind(org).bind(&input.slug).bind(&input.profile.name).bind(&input.profile.description).bind(input.profile.privacy.as_str()).fetch_optional(&mut *tx).await?;
    let Some(team) = team else {
        return Ok(Conflict);
    };
    tx.commit().await?;
    Ok(Applied(team))
}
pub async fn update_organization_team(
    pool: &PgPool,
    actor: i64,
    org: i64,
    team: i64,
    input: &TeamProfile,
) -> Result<OrganizationOutcome<Team>> {
    let mut tx = begin(pool).await?;
    let role = require_org!(tx, actor, org);
    if !administers(role) {
        return Ok(Denied);
    }
    if scoped_team(&mut tx, org, team).await?.is_none() {
        return Ok(Missing);
    }
    let team = sqlx::query_as("UPDATE teams SET name=$3,description=$4,privacy=$5,updated_at=NOW() WHERE id=$1 AND org_id=$2 RETURNING *")
        .bind(team).bind(org).bind(&input.name).bind(&input.description).bind(input.privacy.as_str()).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(team))
}
pub async fn delete_organization_team(
    pool: &PgPool,
    actor: i64,
    org: i64,
    team: i64,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = begin(pool).await?;
    let role = require_org!(tx, actor, org);
    if !administers(role) {
        return Ok(Denied);
    }
    if scoped_team(&mut tx, org, team).await?.is_none() {
        return Ok(Missing);
    }
    let runs: Vec<i64> = sqlx::query_scalar("SELECT id FROM ci_runs WHERE status IN ('queued','running') AND triggered_by_id IN (SELECT user_id FROM team_memberships WHERE team_id=$1) AND repo_id IN (SELECT tra.repo_id FROM team_repo_access tra JOIN repositories r ON r.id=tra.repo_id WHERE tra.team_id=$1 AND r.owner_kind='org' AND r.owner_org_id=$2 AND r.owner_user_id IS NULL)")
        .bind(team).bind(org).fetch_all(&mut *tx).await?;
    super::super::ci::stop_ci_runs(&mut tx, &runs).await?;
    sqlx::query("DELETE FROM team_repo_access WHERE team_id=$1")
        .bind(team)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM team_memberships WHERE team_id=$1")
        .bind(team)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM teams WHERE id=$1 AND org_id=$2")
        .bind(team)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(()))
}
pub async fn list_organization_teams(
    pool: &PgPool,
    actor: i64,
    org: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<TeamListing>>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    let teams = sqlx::query_as("SELECT t.*, (SELECT COUNT(*) FROM team_memberships tm JOIN org_memberships om ON om.user_id=tm.user_id AND om.org_id=t.org_id JOIN users u ON u.id=tm.user_id WHERE tm.team_id=t.id AND tm.role IN ('member','maintainer') AND om.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password) AS member_count FROM teams t WHERE t.org_id=$1 AND t.id>$2 AND (t.privacy='visible' OR (t.privacy='secret' AND ($3 OR EXISTS(SELECT 1 FROM team_memberships m WHERE m.team_id=t.id AND m.user_id=$4 AND m.role IN ('member','maintainer'))))) ORDER BY t.id LIMIT $5")
        .bind(org).bind(page.after).bind(administers(role)).bind(actor).bind(page.limit).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(teams))
}
pub async fn count_organization_teams(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<i64>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    let count = sqlx::query_scalar("SELECT COUNT(*) FROM teams t WHERE t.org_id=$1 AND (t.privacy='visible' OR (t.privacy='secret' AND ($2 OR EXISTS(SELECT 1 FROM team_memberships m WHERE m.team_id=t.id AND m.user_id=$3 AND m.role IN ('member','maintainer')))))")
        .bind(org).bind(administers(role)).bind(actor)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(count))
}
pub async fn list_organization_team_members(
    pool: &PgPool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<TeamMembership>>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    let Some(team) = read_scoped_team(&mut tx, org, team).await? else {
        return Ok(Missing);
    };
    if !team_visible(&mut tx, actor, role, &team).await? {
        return Ok(Denied);
    }
    let members = sqlx::query_as("SELECT tm.* FROM team_memberships tm JOIN org_memberships om ON om.user_id=tm.user_id AND om.org_id=$1 JOIN users u ON u.id=tm.user_id WHERE tm.team_id=$2 AND tm.id>$3 AND tm.role IN ('member','maintainer') AND om.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password ORDER BY tm.id LIMIT $4")
        .bind(org).bind(team.id).bind(page.after).bind(page.limit).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(members))
}
pub async fn add_organization_team_member(
    pool: &PgPool,
    actor: i64,
    org: i64,
    team: i64,
    target: i64,
    role: TeamRole,
) -> Result<OrganizationOutcome<TeamMembership>> {
    let mut tx = begin(pool).await?;
    let actor_role = require_org!(tx, actor, org);
    if !administers(actor_role) {
        return Ok(Denied);
    }
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
    // Also updates member/maintainer; org admins own both roles, not team maintainers.
    let member = sqlx::query_as("INSERT INTO team_memberships(team_id,user_id,role) VALUES($1,$2,$3) ON CONFLICT(team_id,user_id) DO UPDATE SET role=EXCLUDED.role RETURNING *")
        .bind(team).bind(target).bind(role.as_str()).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(member))
}
pub async fn remove_organization_team_member(
    pool: &PgPool,
    actor: i64,
    org: i64,
    team: i64,
    target: i64,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = begin(pool).await?;
    let role = require_org!(tx, actor, org);
    if !administers(role) {
        return Ok(Denied);
    }
    // Exact scoped cleanup, not a grant: target activity/org role is irrelevant.
    if scoped_team(&mut tx, org, team).await?.is_none() {
        return Ok(Missing);
    }
    if sqlx::query("DELETE FROM team_memberships WHERE team_id=$1 AND user_id=$2")
        .bind(team)
        .bind(target)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
    {
        return Ok(Missing);
    }
    // The org membership remains; only this team's own repository grants ended.
    let runs: Vec<i64> = sqlx::query_scalar("SELECT id FROM ci_runs WHERE triggered_by_id=$1 AND status IN ('queued','running') AND repo_id IN (SELECT tra.repo_id FROM team_repo_access tra JOIN repositories r ON r.id=tra.repo_id WHERE tra.team_id=$2 AND r.owner_kind='org' AND r.owner_org_id=$3 AND r.owner_user_id IS NULL)")
        .bind(target).bind(team).bind(org).fetch_all(&mut *tx).await?;
    super::super::ci::stop_ci_runs(&mut tx, &runs).await?;
    tx.commit().await?;
    Ok(Applied(()))
}
pub async fn grant_organization_team_repository(
    pool: &PgPool,
    actor: i64,
    org: i64,
    team: i64,
    repo: i64,
    permission: TeamRepositoryPermission,
) -> Result<OrganizationOutcome<TeamRepoAccess>> {
    let mut tx = begin(pool).await?;
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
    // Replacing permissions requests cancellation conservatively, even if another
    // independent grant still exists. Cancellation is NOT process settlement.
    let old: Option<i16> = sqlx::query_scalar(
        "SELECT permission FROM team_repo_access WHERE team_id=$1 AND repo_id=$2 FOR UPDATE",
    )
    .bind(team)
    .bind(repo)
    .fetch_optional(&mut *tx)
    .await?;
    if old.is_some_and(|level| level > permission.0 || !(1..=5).contains(&level)) {
        cancel_team(&mut tx, org, team, repo).await?;
    }
    let access = sqlx::query_as("INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES($1,$2,$3) ON CONFLICT(team_id,repo_id) DO UPDATE SET permission=EXCLUDED.permission RETURNING *")
        .bind(team).bind(repo).bind(permission.0).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(access))
}
pub async fn revoke_organization_team_repository(
    pool: &PgPool,
    actor: i64,
    org: i64,
    team: i64,
    repo: i64,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = begin(pool).await?;
    let role = require_org!(tx, actor, org);
    if !administers(role) {
        return Ok(Denied);
    }
    if scoped_team(&mut tx, org, team).await?.is_none() {
        return Ok(Missing);
    }
    // Lock own-tenant ownership, including stale lifecycle, for cancellation.
    // Foreign repos are never locked/changed/cancelled by this repair.
    let own_repo = sqlx::query_scalar::<_, i64>("SELECT id FROM repositories WHERE id=$1 AND owner_kind='org' AND owner_org_id=$2 AND owner_user_id IS NULL FOR UPDATE")
        .bind(repo).bind(org).fetch_optional(&mut *tx).await?.is_some();
    if sqlx::query("DELETE FROM team_repo_access WHERE team_id=$1 AND repo_id=$2")
        .bind(team)
        .bind(repo)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
    {
        return Ok(Missing);
    }
    if own_repo {
        cancel_team(&mut tx, org, team, repo).await?;
    }
    tx.commit().await?;
    Ok(Applied(()))
}
pub async fn list_organization_team_repositories(
    pool: &PgPool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<OrganizationOutcome<Vec<TeamRepositoryGrant>>> {
    let mut tx = begin_read(pool).await?;
    let role = require_read_org!(tx, actor, org);
    let Some(team) = read_scoped_team(&mut tx, org, team).await? else {
        return Ok(Missing);
    };
    if !team_visible(&mut tx, actor, role, &team).await? {
        return Ok(Denied);
    }
    // Team visibility is not READ. Apply effective access before LIMIT/projection;
    // same-org team joins match the canonical contribution helper below.
    let rows = sqlx::query_as("SELECT a.id, a.repo_id, a.permission, r.name, o.name AS owner FROM team_repo_access a JOIN repositories r ON r.id=a.repo_id JOIN organizations o ON o.id=r.owner_org_id WHERE a.team_id=$1 AND a.id>$2 AND a.permission BETWEEN 1 AND 5 AND r.owner_kind='org' AND r.owner_org_id=$3 AND r.owner_user_id IS NULL AND ($5 OR NOT r.is_private OR EXISTS(SELECT 1 FROM repo_access d WHERE d.repo_id=r.id AND d.user_id=$6 AND d.access_level BETWEEN 1 AND 5) OR EXISTS(SELECT 1 FROM team_repo_access g JOIN teams t ON t.id=g.team_id AND t.org_id=r.owner_org_id JOIN team_memberships tm ON tm.team_id=t.id AND tm.user_id=$6 AND tm.role IN ('member','maintainer') WHERE g.repo_id=r.id AND g.permission BETWEEN 1 AND 5 AND t.privacy IN ('visible','secret'))) ORDER BY a.id LIMIT $4")
        .bind(team.id).bind(page.after).bind(org).bind(page.limit).bind(administers(role)).bind(actor).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
// Team pages are for active organization members only: anyone else gets the answer of a missing team.
pub async fn get_public_team(
    pool: &PgPool,
    actor: i64,
    org: i64,
    slug: &str,
) -> Result<Option<PublicTeam>> {
    let mut tx = begin_read(pool).await?;
    let Applied(role) = read_authority(&mut tx, actor, org).await? else {
        return Ok(None);
    };
    let Some(team) = sqlx::query_as::<_, Team>("SELECT * FROM teams WHERE org_id=$1 AND slug=$2")
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
    let member_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM team_memberships tm JOIN org_memberships om ON om.user_id=tm.user_id AND om.org_id=$1 JOIN users u ON u.id=tm.user_id WHERE tm.team_id=$2 AND tm.role IN ('member','maintainer') AND om.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(org).bind(team.id).fetch_one(&mut *tx).await?;
    let repository_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM team_repo_access a JOIN repositories r ON r.id=a.repo_id WHERE a.team_id=$1 AND a.permission BETWEEN 1 AND 5 AND r.owner_kind='org' AND r.owner_org_id=$2 AND r.owner_user_id IS NULL AND ($3 OR NOT r.is_private OR EXISTS(SELECT 1 FROM repo_access d WHERE d.repo_id=r.id AND d.user_id=$4 AND d.access_level BETWEEN 1 AND 5) OR EXISTS(SELECT 1 FROM team_repo_access g JOIN teams t ON t.id=g.team_id AND t.org_id=r.owner_org_id JOIN team_memberships tm ON tm.team_id=t.id AND tm.user_id=$4 AND tm.role IN ('member','maintainer') WHERE g.repo_id=r.id AND g.permission BETWEEN 1 AND 5 AND t.privacy IN ('visible','secret')))")
        .bind(team.id).bind(org).bind(administers(role)).bind(actor).fetch_one(&mut *tx).await?;
    let maintainer: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM team_memberships WHERE team_id=$1 AND user_id=$2 AND role='maintainer')")
        .bind(team.id).bind(actor).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(PublicTeam {
        manage: administers(role) || maintainer,
        team,
        member_count,
        repository_count,
    }))
}
pub async fn list_public_team_members(
    pool: &PgPool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<Option<Vec<TeamMembership>>> {
    let mut tx = begin_read(pool).await?;
    let Applied(role) = read_authority(&mut tx, actor, org).await? else {
        return Ok(None);
    };
    let Some(team) = read_scoped_team(&mut tx, org, team).await? else {
        return Ok(None);
    };
    if !team_visible(&mut tx, actor, role, &team).await? {
        return Ok(None);
    }
    let members = sqlx::query_as("SELECT tm.* FROM team_memberships tm JOIN org_memberships om ON om.user_id=tm.user_id AND om.org_id=$1 JOIN users u ON u.id=tm.user_id WHERE tm.team_id=$2 AND tm.id>$3 AND tm.role IN ('member','maintainer') AND om.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password ORDER BY tm.id LIMIT $4")
        .bind(org).bind(team.id).bind(page.after).bind(page.limit).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(members))
}
pub async fn list_public_team_repositories(
    pool: &PgPool,
    actor: i64,
    org: i64,
    team: i64,
    page: OrganizationPage,
) -> Result<Option<Vec<TeamRepositoryGrant>>> {
    let mut tx = begin_read(pool).await?;
    let Applied(role) = read_authority(&mut tx, actor, org).await? else {
        return Ok(None);
    };
    let Some(team) = read_scoped_team(&mut tx, org, team).await? else {
        return Ok(None);
    };
    if !team_visible(&mut tx, actor, role, &team).await? {
        return Ok(None);
    }
    // Same READ filter as list_organization_team_repositories.
    let rows = sqlx::query_as("SELECT a.id, a.repo_id, a.permission, r.name, o.name AS owner FROM team_repo_access a JOIN repositories r ON r.id=a.repo_id JOIN organizations o ON o.id=r.owner_org_id WHERE a.team_id=$1 AND a.id>$2 AND a.permission BETWEEN 1 AND 5 AND r.owner_kind='org' AND r.owner_org_id=$3 AND r.owner_user_id IS NULL AND ($5 OR NOT r.is_private OR EXISTS(SELECT 1 FROM repo_access d WHERE d.repo_id=r.id AND d.user_id=$6 AND d.access_level BETWEEN 1 AND 5) OR EXISTS(SELECT 1 FROM team_repo_access g JOIN teams t ON t.id=g.team_id AND t.org_id=r.owner_org_id JOIN team_memberships tm ON tm.team_id=t.id AND tm.user_id=$6 AND tm.role IN ('member','maintainer') WHERE g.repo_id=r.id AND g.permission BETWEEN 1 AND 5 AND t.privacy IN ('visible','secret'))) ORDER BY a.id LIMIT $4")
        .bind(team.id).bind(page.after).bind(org).bind(page.limit).bind(administers(role)).bind(actor).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(rows))
}
