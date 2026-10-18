use crate::backend::db::{
    models::{AccessLevel, OrgRole, Repository, Team},
    organizations::*,
};
use anyhow::Result;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use OrganizationOutcome::{Applied, Denied, Missing, Suspended};

pub(super) async fn begin(pool: &PgPool) -> Result<Transaction<'_, Postgres>> {
    Ok(pool.begin().await?)
}
// Authorization and projection share a committed snapshot; no locks or effects.
pub(super) async fn begin_read(pool: &PgPool) -> Result<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
pub(super) async fn read_active(connection: &mut PgConnection, user: i64) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT NOT is_banned AND NOT must_change_password FROM users WHERE id=$1",
    )
    .bind(user)
    .fetch_optional(connection)
    .await?
    .unwrap_or(false))
}
/// Whether the account is an active site administrator.
pub(super) async fn site_administrator(connection: &mut PgConnection, actor: i64) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT is_admin AND NOT is_banned AND NOT must_change_password FROM users WHERE id=$1",
    )
    .bind(actor)
    .fetch_optional(connection)
    .await?
    .unwrap_or(false))
}
pub(super) async fn read_authority(
    connection: &mut PgConnection,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<OrgRole>> {
    if sqlx::query_scalar::<_, i64>("SELECT id FROM organizations WHERE id=$1")
        .bind(org)
        .fetch_optional(&mut *connection)
        .await?
        .is_none()
    {
        return Ok(Missing);
    }
    if !read_active(connection, actor).await? {
        return Ok(Denied);
    }
    let role: Option<String> =
        sqlx::query_scalar("SELECT role FROM org_memberships WHERE org_id=$1 AND user_id=$2")
            .bind(org)
            .bind(actor)
            .fetch_optional(connection)
            .await?;
    Ok(match role.and_then(|role| role.parse().ok()) {
        Some(role) => Applied(role),
        None => Denied,
    })
}
macro_rules! require_read_org {
    ($tx:ident, $actor:ident, $org:ident) => {
        match $crate::backend::db::postgres::organizations::authority::read_authority(
            &mut $tx, $actor, $org,
        )
        .await?
        {
            $crate::backend::db::organizations::OrganizationOutcome::Applied(role) => role,
            $crate::backend::db::organizations::OrganizationOutcome::Missing => {
                return Ok($crate::backend::db::organizations::OrganizationOutcome::Missing)
            }
            _ => return Ok($crate::backend::db::organizations::OrganizationOutcome::Denied),
        }
    };
}
pub(super) async fn read_scoped_team(
    connection: &mut PgConnection,
    org: i64,
    team: i64,
) -> Result<Option<Team>> {
    Ok(
        sqlx::query_as("SELECT * FROM teams WHERE id=$1 AND org_id=$2")
            .bind(team)
            .bind(org)
            .fetch_optional(connection)
            .await?,
    )
}
pub(super) async fn active(connection: &mut PgConnection, user: i64) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT NOT is_banned AND NOT must_change_password FROM users WHERE id=$1 FOR SHARE",
    )
    .bind(user)
    .fetch_optional(connection)
    .await?
    .unwrap_or(false))
}
pub(super) async fn membership_role(
    connection: &mut PgConnection,
    org: i64,
    user: i64,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT role FROM org_memberships WHERE org_id=$1 AND user_id=$2 FOR UPDATE",
    )
    .bind(org)
    .bind(user)
    .fetch_optional(connection)
    .await?)
}
pub(in crate::backend::db::postgres) async fn authority(
    connection: &mut PgConnection,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<OrgRole>> {
    // Callers must not lock rows before this: account bans take the exclusive form first.
    crate::backend::db::postgres::users::transitions::lock_membership_shared(connection).await?;
    if sqlx::query_scalar::<_, i64>("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(org)
        .fetch_optional(&mut *connection)
        .await?
        .is_none()
    {
        return Ok(Missing);
    }
    if !active(connection, actor).await? {
        return Ok(Denied);
    }
    let Some(role) = membership_role(connection, org, actor)
        .await?
        .and_then(|role| role.parse().ok())
    else {
        return Ok(Denied);
    };
    // Banning/restricting the last owner remains possible. Tenant writes are
    // suspended instead of treating restricted owners as usable governance.
    if !remaining_active_owner(connection, org, -1).await? {
        return Ok(Suspended);
    }
    Ok(Applied(role))
}
macro_rules! require_org {
    ($tx:ident, $actor:ident, $org:ident) => {
        match $crate::backend::db::postgres::organizations::authority::authority(
            &mut $tx, $actor, $org,
        )
        .await?
        {
            $crate::backend::db::organizations::OrganizationOutcome::Applied(role) => role,
            $crate::backend::db::organizations::OrganizationOutcome::Missing => {
                return Ok($crate::backend::db::organizations::OrganizationOutcome::Missing)
            }
            $crate::backend::db::organizations::OrganizationOutcome::Suspended => {
                return Ok($crate::backend::db::organizations::OrganizationOutcome::Suspended)
            }
            _ => return Ok($crate::backend::db::organizations::OrganizationOutcome::Denied),
        }
    };
}
pub(super) async fn metadata(
    connection: &mut PgConnection,
    org: i64,
) -> Result<OrganizationMetadata> {
    Ok(sqlx::query_as(
        "SELECT o.id,o.name,o.display_name,o.description,o.is_verified,NOT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=o.id AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password) AS governance_suspended FROM organizations o WHERE o.id=$1",
    )
    .bind(org)
    .fetch_one(connection)
    .await?)
}
pub(super) async fn scoped_team(
    connection: &mut PgConnection,
    org: i64,
    team: i64,
) -> Result<Option<Team>> {
    Ok(
        sqlx::query_as("SELECT * FROM teams WHERE id=$1 AND org_id=$2 FOR UPDATE")
            .bind(team)
            .bind(org)
            .fetch_optional(connection)
            .await?,
    )
}
pub(super) async fn team_visible(
    connection: &mut PgConnection,
    actor: i64,
    role: OrgRole,
    team: &Team,
) -> Result<bool> {
    if !matches!(team.privacy.as_str(), "visible" | "secret") {
        return Ok(false);
    }
    if administers(role) || team.privacy == "visible" {
        return Ok(true);
    }
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM team_memberships WHERE team_id=$1 AND user_id=$2 AND role IN ('member','maintainer'))")
        .bind(team.id).bind(actor).fetch_one(connection).await?)
}
pub(super) async fn scoped_repo(
    connection: &mut PgConnection,
    org: i64,
    repo: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT id FROM repositories WHERE id=$1 AND owner_kind='org' AND owner_org_id=$2 AND owner_user_id IS NULL FOR UPDATE")
        .bind(repo).bind(org).fetch_optional(connection).await?.is_some())
}
// The three cancel helpers pick the runs the revocation affects; `stop_ci_runs`
// cancels the queued ones and asks workers to stop the running ones.
pub(super) async fn cancel_actor(
    tx: &mut Transaction<'_, Postgres>,
    org: i64,
    target: i64,
) -> Result<()> {
    let runs: Vec<i64> = sqlx::query_scalar("SELECT id FROM ci_runs WHERE triggered_by_id=$1 AND status IN ('queued','running') AND repo_id IN (SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=$2 AND owner_user_id IS NULL)")
        .bind(target).bind(org).fetch_all(&mut **tx).await?;
    super::super::ci::stop_ci_runs(tx, &runs).await
}
pub(super) async fn cancel_team(
    tx: &mut Transaction<'_, Postgres>,
    org: i64,
    team: i64,
    repo: i64,
) -> Result<()> {
    let runs: Vec<i64> = sqlx::query_scalar("SELECT id FROM ci_runs WHERE repo_id=$1 AND status IN ('queued','running') AND triggered_by_id IN (SELECT user_id FROM team_memberships WHERE team_id=$2) AND EXISTS(SELECT 1 FROM repositories WHERE id=$1 AND owner_kind='org' AND owner_org_id=$3 AND owner_user_id IS NULL)")
        .bind(repo).bind(team).bind(org).fetch_all(&mut **tx).await?;
    super::super::ci::stop_ci_runs(tx, &runs).await
}
pub(super) async fn remaining_active_owner(
    connection: &mut PgConnection,
    org: i64,
    target: i64,
) -> Result<bool> {
    // Org FOR UPDATE serializes tenant role mutations. Lock every other known
    // owner's user/membership facts, including inactive rows, before deciding.
    // Account restrictions and membership changes take the same membership
    // advisory lock before these parent and child rows.
    let owners: Vec<bool> = sqlx::query_scalar("SELECT NOT u.is_banned AND NOT u.must_change_password FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.role='owner' AND m.user_id<>$2 ORDER BY u.id FOR SHARE OF u,m")
        .bind(org).bind(target).fetch_all(connection).await?;
    Ok(owners.into_iter().any(|active| active))
}

pub(crate) async fn organization_repo_access_in_transaction(
    connection: &mut PgConnection,
    user: i64,
    repository: &Repository,
) -> Result<AccessLevel> {
    if repository.owner_kind != "org" || repository.owner_user_id.is_some() {
        return Ok(AccessLevel::None);
    }
    let Some(org) = repository.owner_org_id else {
        return Ok(AccessLevel::None);
    };
    let level: Option<i16> = sqlx::query_scalar("SELECT CASE m.role WHEN 'owner' THEN 6::smallint WHEN 'admin' THEN 5::smallint WHEN 'member' THEN COALESCE((SELECT MAX(a.permission) FROM team_repo_access a JOIN teams t ON t.id=a.team_id AND t.org_id=m.org_id JOIN team_memberships tm ON tm.team_id=t.id AND tm.user_id=m.user_id AND tm.role IN ('member','maintainer') WHERE a.repo_id=r.id AND a.permission BETWEEN 1 AND 5 AND t.privacy IN ('visible','secret')),0::smallint) ELSE 0::smallint END FROM repositories r JOIN organizations o ON o.id=r.owner_org_id JOIN org_memberships m ON m.org_id=o.id AND m.user_id=$1 JOIN users u ON u.id=m.user_id WHERE r.id=$2 AND r.owner_kind='org' AND r.owner_org_id=$3 AND r.owner_user_id IS NULL AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(user).bind(repository.id).bind(org).fetch_optional(&mut *connection).await?;
    let usable: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password)")
        .bind(org).fetch_one(connection).await?;
    // Existing reads remain possible, but org roles cannot authorize writes in
    // suspended governance. Canonical callers must also deny direct-grant writes.
    Ok(AccessLevel::from(if usable {
        level.unwrap_or(0)
    } else {
        level.unwrap_or(0).min(1)
    }))
}
