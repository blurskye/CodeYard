use crate::organizations::profiles::OrganizationMetadata;
use crate::organizations::MAX_SCOPED_REPOSITORIES;
use anyhow::Result;
use codeyard_db_core::models::AccessLevel;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::Team;
use codeyard_db_core::queries::organizations::administers;
use sqlx::Sqlite;
use sqlx::SqliteConnection;
use sqlx::Transaction;

/// Revalidate the initiating credential inside the same immediate transaction as the write.
pub async fn credential(
    connection: &mut SqliteConnection,
    actor: &codeyard_db_core::queries::credentials::Actor,
    org: i64,
    permission: &str,
) -> Result<bool> {
    codeyard_db_core::queries::sqlite::credentials::writes(
        connection,
        actor,
        None,
        Some(org),
        permission,
    )
    .await
}

/// Whether the account is an active site administrator.
pub(super) async fn site_administrator(
    connection: &mut SqliteConnection,
    actor: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT is_admin AND NOT is_banned AND NOT must_change_password FROM users WHERE id=?1",
    )
    .bind(actor)
    .fetch_optional(connection)
    .await?
    .unwrap_or(false))
}

macro_rules! require_org {
    ($tx:ident, $actor:ident, $org:ident) => {
        match codeyard_db_core::queries::sqlite::organizations::authority::authority(
            &mut $tx, $actor, $org,
        )
        .await?
        {
            codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(role) => role,
            codeyard_db_core::queries::organizations::OrganizationOutcome::Missing => {
                return Ok(codeyard_db_core::queries::organizations::OrganizationOutcome::Missing)
            }
            codeyard_db_core::queries::organizations::OrganizationOutcome::Suspended => {
                return Ok(codeyard_db_core::queries::organizations::OrganizationOutcome::Suspended)
            }
            _ => return Ok(codeyard_db_core::queries::organizations::OrganizationOutcome::Denied),
        }
    };
}

pub async fn metadata(connection: &mut SqliteConnection, org: i64) -> Result<OrganizationMetadata> {
    Ok(sqlx::query_as(
        "SELECT o.id,o.name,o.display_name,o.description,o.is_verified,NOT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=o.id AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password) AS governance_suspended,o.avatar_file_id FROM organizations o WHERE o.id=?1",
    )
    .bind(org)
    .fetch_one(connection)
    .await?)
}
pub(super) async fn scoped_team(
    connection: &mut SqliteConnection,
    org: i64,
    team: i64,
) -> Result<Option<Team>> {
    Ok(
        sqlx::query_as("SELECT * FROM teams WHERE id=?1 AND org_id=?2")
            .bind(team)
            .bind(org)
            .fetch_optional(connection)
            .await?,
    )
}
pub async fn team_visible(
    connection: &mut SqliteConnection,
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
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM team_memberships WHERE team_id=?1 AND user_id=?2 AND role IN ('member','maintainer'))")
        .bind(team.id).bind(actor).fetch_one(connection).await?)
}
pub async fn scoped_repo(connection: &mut SqliteConnection, org: i64, repo: i64) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT id FROM repositories WHERE id=?1 AND owner_kind='org' AND owner_org_id=?2 AND owner_user_id IS NULL")
        .bind(repo).bind(org).fetch_optional(connection).await?.is_some())
}
/// All of `ids` are repositories of `org`. At most `MAX_SCOPED_REPOSITORIES` distinct ids are
/// accepted so one selection stays one bounded query.
pub(super) async fn scoped_repos(
    connection: &mut SqliteConnection,
    org: i64,
    ids: &[i64],
) -> Result<bool> {
    let ids: std::collections::BTreeSet<i64> = ids.iter().copied().collect();
    if ids.len() > MAX_SCOPED_REPOSITORIES {
        return Ok(false);
    }
    let found: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repositories WHERE id IN (SELECT value FROM json_each(?1)) AND owner_kind='org' AND owner_org_id=?2 AND owner_user_id IS NULL")
        .bind(serde_json::to_string(&ids)?).bind(org).fetch_one(connection).await?;
    Ok(found == ids.len() as i64)
}
// The cancel helpers pick the runs the revocation affects; `stop_ci_runs`
// cancels the queued ones and asks workers to stop the running ones.
pub(super) async fn cancel_actor(
    tx: &mut Transaction<'_, Sqlite>,
    org: i64,
    target: i64,
) -> Result<()> {
    let runs: Vec<i64> = sqlx::query_scalar("SELECT id FROM ci_runs WHERE triggered_by_id=?1 AND (status IN ('queued','running') OR completed_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour')) AND repo_id IN (SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=?2 AND owner_user_id IS NULL)")
        .bind(target).bind(org).fetch_all(&mut **tx).await?;
    codeyard_db_core::queries::sqlite::ci::stop_ci_runs(tx, &runs).await
}
pub(super) async fn cancel_team(
    tx: &mut Transaction<'_, Sqlite>,
    org: i64,
    team: i64,
    repo: i64,
) -> Result<()> {
    let runs: Vec<i64> = sqlx::query_scalar("SELECT id FROM ci_runs WHERE repo_id=?1 AND (status IN ('queued','running') OR completed_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour')) AND triggered_by_id IN (SELECT user_id FROM effective_team_memberships WHERE team_id=?2) AND EXISTS(SELECT 1 FROM repositories WHERE id=?1 AND owner_kind='org' AND owner_org_id=?3 AND owner_user_id IS NULL)")
        .bind(repo).bind(team).bind(org).fetch_all(&mut **tx).await?;
    codeyard_db_core::queries::sqlite::ci::stop_ci_runs(tx, &runs).await
}

pub(crate) async fn organization_repo_access_in_transaction(
    connection: &mut SqliteConnection,
    user: i64,
    repository: &Repository,
) -> Result<AccessLevel> {
    if repository.owner_kind != "org" || repository.owner_user_id.is_some() {
        return Ok(AccessLevel::None);
    }
    let Some(org) = repository.owner_org_id else {
        return Ok(AccessLevel::None);
    };
    let level: Option<i16> = sqlx::query_scalar("SELECT MAX(CASE m.role WHEN 'owner' THEN 6 WHEN 'admin' THEN 5 WHEN 'member' THEN COALESCE((SELECT MAX(a.permission) FROM team_repo_access a JOIN teams t ON t.id=a.team_id AND t.org_id=m.org_id JOIN effective_team_memberships tm ON tm.team_id=t.id AND tm.user_id=m.user_id AND tm.role IN ('member','maintainer') WHERE a.repo_id=r.id AND a.permission BETWEEN 1 AND 5 AND t.privacy IN ('visible','secret')),0) ELSE 0 END,COALESCE((SELECT MAX(role.base_level) FROM organization_role_assignments assigned JOIN organization_roles role ON role.id=assigned.role_id AND role.base_level BETWEEN 1 AND 5 WHERE assigned.org_id=m.org_id AND ((assigned.user_id=m.user_id AND assigned.team_id IS NULL) OR (assigned.user_id IS NULL AND EXISTS(SELECT 1 FROM teams t JOIN effective_team_memberships tm ON tm.team_id=t.id WHERE t.id=assigned.team_id AND t.org_id=assigned.org_id AND t.privacy IN ('visible','secret') AND tm.user_id=m.user_id AND tm.role IN ('member','maintainer'))))),0),COALESCE((SELECT s.default_repository_permission FROM organization_settings s WHERE s.org_id=m.org_id AND s.default_repository_permission IN (1,3,5)),0)) FROM repositories r JOIN organizations o ON o.id=r.owner_org_id JOIN org_memberships m ON m.org_id=o.id AND m.user_id=?1 JOIN users u ON u.id=m.user_id WHERE r.id=?2 AND r.owner_kind='org' AND r.owner_org_id=?3 AND r.owner_user_id IS NULL AND NOT u.is_banned AND NOT u.must_change_password AND m.role IN ('owner','admin','member') AND NOT EXISTS(SELECT 1 FROM organization_blocks b WHERE b.org_id=m.org_id AND b.user_id=m.user_id)")
        .bind(user).bind(repository.id).bind(org).fetch_optional(&mut *connection).await?;
    let usable = remaining_active_owner(connection, org, -1).await?;
    // Preserve existing reads; suspended governance cannot grant org-role writes.
    // Canonical callers must additionally refuse direct-grant write admission.
    Ok(AccessLevel::from(if usable {
        level.unwrap_or(0)
    } else {
        level.unwrap_or(0).min(1)
    }))
}

use codeyard_db_core::queries::sqlite::organizations::authority::remaining_active_owner;
