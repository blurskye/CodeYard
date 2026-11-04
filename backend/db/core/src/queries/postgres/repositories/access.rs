use crate::models::AccessLevel;
use crate::queries::postgres::users::transitions::lock_accounts_shared;
use anyhow::Result;
use sqlx::PgConnection;
/// Effectful callers lock the rows they depend on in their own transaction.
/// A pool projection alone is never a write permit.
pub async fn user_repo_access_in_transaction(
    connection: &mut PgConnection,
    user_id: i64,
    repo_id: i64,
) -> Result<AccessLevel> {
    // Projection only, not a write permit. All contributions and governance
    // share one statement snapshot; suspended orgs cap even nonmember grants.
    // Collaborator levels are 1..=5; Owner comes only from scoped ownership.
    let level: Option<i16> = sqlx::query_scalar(&format!(
        "SELECT ({USER_REPO_ACCESS_SQL}) FROM repositories r JOIN users u ON u.id = $1 WHERE r.id = $2"
    ))
    .bind(user_id)
    .bind(repo_id)
    .fetch_optional(connection)
    .await?;
    Ok(AccessLevel::from(level.unwrap_or(0)))
}

// The actor's access level on repository `r`, 0 when none. A plain scalar over the
// outer `r` and `u` rows, so the run list can evaluate it once per repository.
pub const USER_REPO_ACCESS_SQL: &str = r#"
    (CASE WHEN NOT r.is_gist AND NOT u.is_banned AND NOT u.must_change_password
        AND ((r.owner_kind = 'user' AND r.owner_user_id IS NOT NULL AND r.owner_org_id IS NULL)
            OR (r.owner_kind = 'org' AND r.owner_user_id IS NULL AND r.owner_org_id IS NOT NULL
                AND EXISTS (SELECT 1 FROM organizations o WHERE o.id = r.owner_org_id)))
        AND NOT EXISTS (SELECT 1 FROM organization_blocks b WHERE b.org_id=r.owner_org_id AND b.user_id=u.id)
        THEN LEAST(GREATEST(
            CASE WHEN r.owner_kind = 'user' AND r.owner_user_id = u.id
                AND r.owner_org_id IS NULL THEN 6 ELSE 0 END,
            CASE WHEN NOT r.is_private THEN 1 ELSE 0 END,
            COALESCE((SELECT MAX(a.access_level) FROM github_apps a
                JOIN github_app_installations i ON i.app_id=a.id
                JOIN users owner ON owner.id=a.owner_id
                WHERE a.bot_id=u.id AND i.suspended_at IS NULL
                    AND NOT owner.is_banned AND NOT owner.must_change_password
                    AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id)
                    AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar
                        WHERE ar.installation_id=i.id AND ar.repo_id=r.id))),0),
            COALESCE((SELECT d.access_level FROM repo_access d
                WHERE d.repo_id = r.id AND d.user_id = u.id
                    AND d.access_level BETWEEN 1 AND 5), 0),
            COALESCE((SELECT s.default_repository_permission FROM organization_settings s
                JOIN org_memberships member ON member.org_id=s.org_id AND member.user_id=u.id
                    AND member.role IN ('owner','admin','member')
                WHERE s.org_id=r.owner_org_id AND s.default_repository_permission IN (1,3,5)),0),
            COALESCE((SELECT MAX(role.base_level) FROM organization_role_assignments a
                JOIN organization_roles role ON role.id=a.role_id AND role.base_level BETWEEN 1 AND 5
                WHERE a.org_id=r.owner_org_id
                    AND EXISTS(SELECT 1 FROM org_memberships member WHERE member.org_id=a.org_id
                        AND member.user_id=u.id AND member.role IN ('owner','admin','member'))
                    AND ((a.user_id=u.id AND a.team_id IS NULL) OR (a.user_id IS NULL AND EXISTS(
                        SELECT 1 FROM teams t JOIN team_ancestors lineage ON lineage.ancestor_id=t.id
                        JOIN team_memberships tm ON tm.team_id=lineage.team_id
                        WHERE t.id=a.team_id AND t.org_id=a.org_id AND t.privacy IN ('visible','secret')
                            AND tm.user_id=u.id AND tm.role IN ('member','maintainer'))))),0),
            COALESCE((
                SELECT CASE m.role WHEN 'owner' THEN 6 WHEN 'admin' THEN 5
                    WHEN 'member' THEN COALESCE((
                        SELECT MAX(a.permission) FROM team_repo_access a
                        JOIN teams t ON t.id = a.team_id AND t.org_id = m.org_id
                        JOIN team_ancestors lineage ON lineage.ancestor_id=t.id
                        JOIN team_memberships tm ON tm.team_id=lineage.team_id
                            AND tm.user_id = u.id AND tm.role IN ('member','maintainer')
                        WHERE a.repo_id = r.id AND a.permission BETWEEN 1 AND 5
                            AND t.privacy IN ('visible','secret')
                    ), 0) ELSE 0 END
                FROM organizations o JOIN org_memberships m ON m.org_id = o.id
                WHERE r.owner_kind = 'org' AND r.owner_user_id IS NULL
                    AND o.id = r.owner_org_id AND m.user_id = u.id
            ), 0)),
            CASE WHEN r.owner_kind = 'org' AND NOT EXISTS (
                    SELECT 1 FROM organizations o
                    JOIN org_memberships m ON m.org_id = o.id AND m.role = 'owner'
                    JOIN users own ON own.id = m.user_id
                    WHERE o.id = r.owner_org_id
                        AND NOT own.is_banned AND NOT own.must_change_password)
                THEN 1 ELSE 6 END)
        ELSE 0 END)::smallint
    "#;

/// Whether repository `r` is readable by the viewer, whom the query joins as `u` (NULL for an
/// anonymous visitor). It follows `repositories::access::can_read`: public repositories, the
/// personal repositories of a site admin, and the access projection for everything else.
pub fn readable_condition() -> String {
    format!(
        "(NOT r.is_gist AND (NOT r.is_private OR (r.owner_kind = 'user' AND u.is_admin AND NOT u.is_banned AND NOT u.must_change_password) OR ({USER_REPO_ACCESS_SQL}) >= 1))"
    )
}

/// Read authority includes archived repositories and does not require interaction privileges.
/// Keep the supplying authority locks until the caller has loaded its protected payload.
pub async fn lock_readable_repository_in_transaction(
    connection: &mut PgConnection,
    user_id: i64,
    repo_id: i64,
    repository_lock: RepositoryRowLock,
) -> Result<bool> {
    if lock_user_repo_access_in_transaction(connection, user_id, repo_id, repository_lock, true)
        .await?
        .is_none()
    {
        return Ok(false);
    }
    Ok(sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=$1 WHERE r.id=$2 AND {})",
        readable_condition()
    ))
    .bind(user_id)
    .bind(repo_id)
    .fetch_one(connection)
    .await?)
}

#[derive(Clone, Copy)]
pub enum RepositoryRowLock {
    Shared,
    Exclusive,
}

pub(crate) struct LockedRepositoryAccess {
    pub access: AccessLevel,
    pub is_admin: bool,
    pub owner_user_id: Option<i64>,
    pub organization: bool,
    pub organization_usable: bool,
}

/// The owner, archival state and source of a repository row, read before any lock.
type RepositoryScopeRow = (String, Option<i64>, Option<i64>, bool, Option<i64>);

/// Retain the actor's account, then lock the authority scope in parent-to-child order:
/// organization, account, the fork's sources (see `lock_fork_ancestors`), repository, then
/// memberships and grants. Mutation paths use the same rows.
pub async fn lock_user_repo_access_in_transaction(
    connection: &mut PgConnection,
    user_id: i64,
    repo_id: i64,
    repository_lock: RepositoryRowLock,
    include_archived: bool,
) -> Result<Option<LockedRepositoryAccess>> {
    lock_accounts_shared(connection, &[user_id]).await?;
    let scope: Option<RepositoryScopeRow> = sqlx::query_as(
        "SELECT owner_kind,owner_user_id,owner_org_id,is_archived,forked_from_id FROM repositories WHERE id=$1 AND NOT is_gist",
    )
    .bind(repo_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some((owner_kind, owner_user_id, owner_org_id, archived, forked_from)) = scope else {
        return Ok(None);
    };
    if archived && !include_archived {
        return Ok(None);
    }
    // Transfers lock the owning and ancestor organizations as one ordered set.
    let _: Vec<i64> = sqlx::query_scalar("WITH RECURSIVE family AS (SELECT id,owner_org_id,forked_from_id FROM repositories WHERE id=$1 UNION SELECT r.id,r.owner_org_id,r.forked_from_id FROM repositories r JOIN family f ON r.id=f.forked_from_id) SELECT id FROM organizations WHERE id IN (SELECT owner_org_id FROM family) ORDER BY id FOR SHARE")
        .bind(repo_id).fetch_all(&mut *connection).await?;
    let organization = match (owner_kind.as_str(), owner_user_id, owner_org_id) {
        ("user", Some(_), None) => false,
        ("org", None, Some(org_id)) => {
            if sqlx::query_scalar::<_, i64>("SELECT id FROM organizations WHERE id=$1 FOR SHARE")
                .bind(org_id)
                .fetch_optional(&mut *connection)
                .await?
                .is_none()
            {
                return Ok(None);
            }
            true
        }
        _ => return Ok(None),
    };

    let actor: Option<(bool, bool, bool)> = sqlx::query_as(
        "SELECT is_admin,is_banned,must_change_password FROM users WHERE id=$1 FOR SHARE",
    )
    .bind(user_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some((is_admin, false, false)) = actor else {
        return Ok(None);
    };

    if !organization {
        let owner = owner_user_id.expect("validated personal repository owner");
        // Only the owner's existence matters, so a key share lock is enough. A share
        // lock would wait for the FOR NO KEY UPDATE an admin takes on their own row
        // before banning someone, and that ban may in turn wait for this actor.
        if owner != user_id
            && sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE id=$1 FOR KEY SHARE")
                .bind(owner)
                .fetch_optional(&mut *connection)
                .await?
                .is_none()
        {
            return Ok(None);
        }
    }

    // The fork's sources come before the fork row (see lock_fork_ancestors). Exclusive
    // access takes them FOR NO KEY UPDATE, the lock a fork deletion needs on its source.
    if forked_from.is_some() {
        let source_lock = match repository_lock {
            RepositoryRowLock::Shared => "FOR SHARE",
            RepositoryRowLock::Exclusive => "FOR NO KEY UPDATE",
        };
        crate::queries::postgres::repositories::records::lock_fork_ancestors(
            connection,
            repo_id,
            source_lock,
        )
        .await?;
    }

    let repository_sql = match (repository_lock, include_archived) {
        (RepositoryRowLock::Shared, true) => "SELECT id FROM repositories WHERE id=$1 AND owner_kind=$2 AND owner_user_id IS NOT DISTINCT FROM $3 AND owner_org_id IS NOT DISTINCT FROM $4 FOR SHARE",
        (RepositoryRowLock::Shared, false) => "SELECT id FROM repositories WHERE id=$1 AND owner_kind=$2 AND owner_user_id IS NOT DISTINCT FROM $3 AND owner_org_id IS NOT DISTINCT FROM $4 AND NOT is_archived FOR SHARE",
        (RepositoryRowLock::Exclusive, true) => "SELECT id FROM repositories WHERE id=$1 AND owner_kind=$2 AND owner_user_id IS NOT DISTINCT FROM $3 AND owner_org_id IS NOT DISTINCT FROM $4 FOR UPDATE",
        (RepositoryRowLock::Exclusive, false) => "SELECT id FROM repositories WHERE id=$1 AND owner_kind=$2 AND owner_user_id IS NOT DISTINCT FROM $3 AND owner_org_id IS NOT DISTINCT FROM $4 AND NOT is_archived FOR UPDATE",
    };
    if sqlx::query_scalar::<_, i64>(repository_sql)
        .bind(repo_id)
        .bind(&owner_kind)
        .bind(owner_user_id)
        .bind(owner_org_id)
        .fetch_optional(&mut *connection)
        .await?
        .is_none()
    {
        return Ok(None);
    }

    let migration_locked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repository_migration_locks WHERE repo_id=$1)",
    )
    .bind(repo_id)
    .fetch_one(&mut *connection)
    .await?;
    if migration_locked && !include_archived {
        return Ok(None);
    }

    let organization_usable = if let Some(org_id) = owner_org_id {
        let _: Option<String> = sqlx::query_scalar(
            "SELECT role FROM org_memberships WHERE org_id=$1 AND user_id=$2 FOR SHARE",
        )
        .bind(org_id)
        .bind(user_id)
        .fetch_optional(&mut *connection)
        .await?;
        let usable: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password)",
        )
        .bind(org_id)
        .fetch_one(&mut *connection)
        .await?;

        let team_ids: Vec<i64> = sqlx::query_scalar(
            "SELECT t.id FROM teams t WHERE t.org_id=$1 AND t.privacy IN ('visible','secret') AND EXISTS(SELECT 1 FROM team_memberships m WHERE m.team_id=t.id AND m.user_id=$2 AND m.role IN ('member','maintainer')) AND EXISTS(SELECT 1 FROM team_repo_access a WHERE a.team_id=t.id AND a.repo_id=$3 AND a.permission BETWEEN 1 AND 5) ORDER BY t.id FOR SHARE OF t",
        )
        .bind(org_id)
        .bind(user_id)
        .bind(repo_id)
        .fetch_all(&mut *connection)
        .await?;
        if !team_ids.is_empty() {
            let _: Vec<i64> = sqlx::query_scalar(
                "SELECT team_id FROM team_memberships WHERE user_id=$1 AND team_id=ANY($2) AND role IN ('member','maintainer') ORDER BY team_id FOR SHARE",
            )
            .bind(user_id)
            .bind(&team_ids)
            .fetch_all(&mut *connection)
            .await?;
            let _: Vec<i64> = sqlx::query_scalar(
                "SELECT team_id FROM team_repo_access WHERE repo_id=$1 AND team_id=ANY($2) AND permission BETWEEN 1 AND 5 ORDER BY team_id FOR SHARE",
            )
            .bind(repo_id)
            .bind(&team_ids)
            .fetch_all(&mut *connection)
            .await?;
        }
        usable
    } else {
        true
    };

    let _: Option<i16> = sqlx::query_scalar(
        "SELECT access_level FROM repo_access WHERE user_id=$1 AND repo_id=$2 FOR SHARE",
    )
    .bind(user_id)
    .bind(repo_id)
    .fetch_optional(&mut *connection)
    .await?;

    // Trusted bot calls may have no token whose issuer lock retains the App owner.
    // Discovering that owner here is too late to wait in account/row lock order.
    // Retain its availability without waiting; the projection below rechecks it.
    sqlx::query("SELECT id FROM users WHERE id IN (SELECT owner_id FROM github_apps WHERE bot_id=$1) ORDER BY id FOR SHARE NOWAIT")
        .bind(user_id).execute(&mut *connection).await?;

    // Retain the installation supplying App-derived authority until the effect commits.
    // Recheck its bounds in the subsequent projection after any revocation lock wait.
    let _: Vec<i64> = sqlx::query_scalar("SELECT i.id FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id WHERE a.bot_id=$1 AND (i.user_id=$2 OR i.org_id=$3) ORDER BY i.id FOR SHARE OF i")
        .bind(user_id).bind(owner_user_id).bind(owner_org_id)
        .fetch_all(&mut *connection).await?;
    let access = user_repo_access_in_transaction(connection, user_id, repo_id).await?;
    Ok(Some(LockedRepositoryAccess {
        access,
        is_admin,
        owner_user_id,
        organization,
        organization_usable,
    }))
}
