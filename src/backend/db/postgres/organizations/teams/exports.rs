//! Direct edges for migration archives; effective memberships must not become new grants.
use super::*;

pub async fn list_direct_organization_team_members(
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
    let members = sqlx::query_as("SELECT tm.* FROM team_memberships tm JOIN org_memberships om ON om.user_id=tm.user_id AND om.org_id=$1 WHERE tm.team_id=$2 AND tm.id>$3 AND tm.role IN ('member','maintainer') AND om.role IN ('owner','admin','member') ORDER BY tm.id LIMIT $4")
        .bind(org)
        .bind(team.id)
        .bind(page.after)
        .bind(page.limit)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(members))
}

pub async fn list_direct_organization_team_repositories(
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
    let sql =
        team_repositories_page_sql().replace("effective_team_repo_access", "team_repo_access");
    let rows = sqlx::query_as(&sql)
        .bind(team.id)
        .bind(page.after)
        .bind(org)
        .bind(page.limit)
        .bind(administers(role))
        .bind(actor)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
