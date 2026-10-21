use crate::backend::db::models::{AccessLevel, RepoAccess, Repository, User};
use crate::backend::db::postgres::users::transitions::lock_membership_shared;
use crate::backend::db::repositories::access::CollaboratorChange;
use anyhow::Result;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

pub async fn get_user_repo_access(
    pool: &PgPool,
    user_id: i64,
    repo_id: i64,
) -> Result<AccessLevel> {
    let mut connection = pool.acquire().await?;
    user_repo_access_in_transaction(&mut connection, user_id, repo_id).await
}

pub async fn get_users_repo_access(
    pool: &PgPool,
    user_ids: &[i64],
    repo_id: i64,
) -> Result<Vec<(i64, AccessLevel)>> {
    let rows: Vec<(i64, i16)> = sqlx::query_as(&format!("SELECT u.id, ({USER_REPO_ACCESS_SQL}) FROM repositories r CROSS JOIN users u WHERE r.id = $2 AND u.id = ANY($1)"))
        .bind(user_ids).bind(repo_id).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(id, level)| (id, AccessLevel::from(level)))
        .collect())
}

pub async fn readable_repository_ids(
    pool: &PgPool,
    user_id: Option<i64>,
    repo_ids: &[i64],
) -> Result<Vec<i64>> {
    anyhow::ensure!(repo_ids.len() <= 100, "Repository page exceeds its limit");
    Ok(sqlx::query_scalar(&format!("SELECT r.id FROM repositories r LEFT JOIN users u ON u.id=$1 WHERE r.id=ANY($2) AND (NOT r.is_private OR (r.owner_kind='user' AND (u.is_admin OR r.owner_user_id=u.id)) OR ({USER_REPO_ACCESS_SQL}) >= 1)"))
        .bind(user_id).bind(repo_ids).fetch_all(pool).await?)
}

/// Effectful callers lock the rows they depend on in their own transaction.
/// A pool projection alone is never a write permit.
pub(crate) async fn user_repo_access_in_transaction(
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
pub(crate) const USER_REPO_ACCESS_SQL: &str = r#"
    (CASE WHEN NOT u.is_banned AND NOT u.must_change_password
        AND ((r.owner_kind = 'user' AND r.owner_user_id IS NOT NULL AND r.owner_org_id IS NULL)
            OR (r.owner_kind = 'org' AND r.owner_user_id IS NULL AND r.owner_org_id IS NOT NULL
                AND EXISTS (SELECT 1 FROM organizations o WHERE o.id = r.owner_org_id)))
        THEN LEAST(GREATEST(
            CASE WHEN r.owner_kind = 'user' AND r.owner_user_id = u.id
                AND r.owner_org_id IS NULL THEN 6 ELSE 0 END,
            CASE WHEN NOT r.is_private THEN 1 ELSE 0 END,
            COALESCE((SELECT d.access_level FROM repo_access d
                WHERE d.repo_id = r.id AND d.user_id = u.id
                    AND d.access_level BETWEEN 1 AND 5), 0),
            COALESCE((
                SELECT CASE m.role WHEN 'owner' THEN 6 WHEN 'admin' THEN 5
                    WHEN 'member' THEN COALESCE((
                        SELECT MAX(a.permission) FROM team_repo_access a
                        JOIN teams t ON t.id = a.team_id AND t.org_id = m.org_id
                        JOIN team_memberships tm ON tm.team_id = t.id
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
pub(crate) fn readable_condition() -> String {
    format!(
        "(NOT r.is_private OR (r.owner_kind = 'user' AND u.is_admin) OR ({USER_REPO_ACCESS_SQL}) >= 1)"
    )
}

/// Current visibility and metadata in one snapshot for a bounded run list.
pub async fn readable_repositories_by_ids(
    pool: &PgPool,
    user_id: i64,
    ids: &[i64],
) -> Result<Vec<Repository>> {
    debug_assert!(ids.len() <= 500);
    Ok(sqlx::query_as(&format!(
        "SELECT r.* FROM repositories r JOIN users u ON u.id = $1 WHERE r.id = ANY($2) AND NOT u.is_banned AND NOT u.must_change_password AND {}",
        readable_condition()
    ))
    .bind(user_id)
    .bind(ids)
    .fetch_all(pool)
    .await?)
}

#[derive(Clone, Copy)]
pub(crate) enum RepositoryRowLock {
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

/// Take the shared membership lock, then lock the authority scope in parent-to-child order:
/// organization, account, the fork's sources (see `lock_fork_ancestors`), repository, then
/// memberships and grants. Mutation paths use the same rows.
pub(crate) async fn lock_user_repo_access_in_transaction(
    connection: &mut PgConnection,
    user_id: i64,
    repo_id: i64,
    repository_lock: RepositoryRowLock,
    include_archived: bool,
) -> Result<Option<LockedRepositoryAccess>> {
    // Before any row lock. A ban holds this lock exclusively while it locks the owner's
    // organizations and account, so rows taken before it here could deadlock with that ban.
    lock_membership_shared(connection).await?;
    let scope: Option<RepositoryScopeRow> = sqlx::query_as(
        "SELECT owner_kind,owner_user_id,owner_org_id,is_archived,forked_from_id FROM repositories WHERE id=$1",
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
        super::records::lock_fork_ancestors(connection, repo_id, source_lock).await?;
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

    let organization_usable = if let Some(org_id) = owner_org_id {
        let _: Option<String> = sqlx::query_scalar(
            "SELECT role FROM org_memberships WHERE org_id=$1 AND user_id=$2 FOR SHARE",
        )
        .bind(org_id)
        .bind(user_id)
        .fetch_optional(&mut *connection)
        .await?;
        let owners: Vec<bool> = sqlx::query_scalar(
            "SELECT NOT u.is_banned AND NOT u.must_change_password FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.role='owner' ORDER BY u.id FOR SHARE OF u,m",
        )
        .bind(org_id)
        .fetch_all(&mut *connection)
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
        owners.into_iter().any(|active| active)
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

    let access = user_repo_access_in_transaction(connection, user_id, repo_id).await?;
    Ok(Some(LockedRepositoryAccess {
        access,
        is_admin,
        owner_user_id,
        organization,
        organization_usable,
    }))
}

async fn lock_access_repository(connection: &mut PgConnection, repo_id: i64) -> Result<bool> {
    lock_membership_shared(&mut *connection).await?;
    // Conflicts with authority readers even when no direct grant existed at their first read.
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
            .bind(repo_id)
            .fetch_optional(connection)
            .await?
            .is_some(),
    )
}

async fn grant_repo_access_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    user_id: i64,
    repo_id: i64,
    level: AccessLevel,
) -> Result<RepoAccess> {
    let previous: Option<i16> = sqlx::query_scalar(
        "SELECT access_level FROM repo_access WHERE user_id = $1 AND repo_id = $2 FOR UPDATE",
    )
    .bind(user_id)
    .bind(repo_id)
    .fetch_optional(&mut **tx)
    .await?;
    let result = sqlx::query_as::<_, RepoAccess>(
        r#"
        INSERT INTO repo_access (user_id, repo_id, access_level)
        VALUES ($1, $2, $3)
        ON CONFLICT(user_id, repo_id) DO UPDATE SET access_level = EXCLUDED.access_level
        RETURNING *
        "#,
    )
    .bind(user_id)
    .bind(repo_id)
    .bind(level as i16)
    .fetch_one(&mut **tx)
    .await?;
    if previous.is_some_and(|previous| previous > level as i16) {
        super::super::ci::stop_actor_repo_ci_runs(tx, user_id, repo_id).await?;
    }
    Ok(result)
}

async fn revoke_repo_access_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    user_id: i64,
    repo_id: i64,
) -> Result<bool> {
    let result = sqlx::query("DELETE FROM repo_access WHERE user_id = $1 AND repo_id = $2")
        .bind(user_id)
        .bind(repo_id)
        .execute(&mut **tx)
        .await?;
    let removed = result.rows_affected() > 0;
    if removed {
        // The collaborator's queued runs here are cancelled and running ones are
        // asked to stop, like the organization revocations do.
        super::super::ci::stop_actor_repo_ci_runs(tx, user_id, repo_id).await?;
    }
    Ok(removed)
}

pub async fn change_personal_repository_collaborator(
    pool: &PgPool,
    actor: i64,
    repo_id: i64,
    username: &str,
    permission: Option<AccessLevel>,
) -> Result<CollaboratorChange> {
    let mut tx = pool.begin().await?;
    lock_membership_shared(&mut tx).await?;
    let actor_state: Option<(bool, bool)> =
        sqlx::query_as("SELECT is_banned,must_change_password FROM users WHERE id=$1 FOR SHARE")
            .bind(actor)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((is_banned, must_change_password)) = actor_state else {
        return Ok(CollaboratorChange::NotFound);
    };
    if must_change_password {
        return Ok(CollaboratorChange::PasswordChangeRequired);
    }
    if is_banned {
        return Ok(CollaboratorChange::NotFound);
    }
    let target = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username=$1 FOR SHARE")
        .bind(username)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(target) = target else {
        return Ok(CollaboratorChange::NotFound);
    };
    if permission.is_some() && (target.is_banned || target.must_change_password) {
        return Ok(CollaboratorChange::NotFound);
    }
    let owned = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM repositories WHERE id=$1 AND owner_kind='user' AND owner_user_id=$2 AND owner_org_id IS NULL FOR UPDATE",
    )
    .bind(repo_id)
    .bind(actor)
    .fetch_optional(&mut *tx)
    .await?
    .is_some();
    if !owned {
        return Ok(CollaboratorChange::NotFound);
    }
    if permission.is_some() && target.id == actor {
        return Ok(CollaboratorChange::InvalidCollaboratorOwner);
    }
    match permission {
        Some(level) => {
            grant_repo_access_in_transaction(&mut tx, target.id, repo_id, level).await?;
        }
        None => {
            revoke_repo_access_in_transaction(&mut tx, target.id, repo_id).await?;
        }
    }
    tx.commit().await?;
    Ok(CollaboratorChange::Applied(Box::new(target)))
}

pub async fn grant_repo_access(
    pool: &PgPool,
    user_id: i64,
    repo_id: i64,
    level: AccessLevel,
) -> Result<RepoAccess> {
    let mut tx = pool.begin().await?;
    lock_access_repository(&mut tx, repo_id).await?;
    let result = grant_repo_access_in_transaction(&mut tx, user_id, repo_id, level).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn list_repo_collaborators(pool: &PgPool, repo_id: i64) -> Result<Vec<RepoAccess>> {
    let results = sqlx::query_as::<_, RepoAccess>(
        "SELECT * FROM repo_access WHERE repo_id = $1 ORDER BY access_level DESC",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;

    Ok(results)
}

/// Current writers among a supplied set, using the same access projection as individual checks.
pub async fn write_access_user_ids(pool: &PgPool, repo_id: i64, ids: &[i64]) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT u.id FROM repositories r JOIN users u ON u.id = ANY($2)
         WHERE r.id = $1 AND NOT r.is_archived AND NOT u.is_banned AND NOT u.must_change_password
         AND ((r.owner_kind = 'user' AND (u.is_admin OR r.owner_user_id = u.id)) OR ({USER_REPO_ACCESS_SQL}) >= $3)"
    )).bind(repo_id).bind(ids).bind(AccessLevel::Write as i16).fetch_all(pool).await?)
}

/// Up to 30 users other than `viewer_id` who can write to the repository, most privileged first.
/// The candidates are the owner, the collaborators and the organization members. Each level is
/// the effective access from `USER_REPO_ACCESS_SQL`, so teams and banned accounts count as they do
/// for writes.
pub async fn list_write_access_users(
    pool: &PgPool,
    repo_id: i64,
    viewer_id: i64,
) -> Result<Vec<(String, AccessLevel)>> {
    let rows: Vec<(String, i16)> = sqlx::query_as(&format!(
        "SELECT username, level FROM (
            SELECT u.username, ({USER_REPO_ACCESS_SQL}) AS level
            FROM repositories r JOIN users u ON u.id IN (
                SELECT owner_user_id FROM repositories WHERE id = $1 AND owner_user_id IS NOT NULL
                UNION SELECT user_id FROM repo_access WHERE repo_id = $1
                UNION SELECT m.user_id FROM repositories o JOIN org_memberships m ON m.org_id = o.owner_org_id
                    WHERE o.id = $1 AND o.owner_user_id IS NULL
            )
            WHERE r.id = $1 AND u.id <> $2
        ) candidates
        WHERE level >= $3 ORDER BY level DESC, username LIMIT 30"
    ))
    .bind(repo_id)
    .bind(viewer_id)
    .bind(AccessLevel::Write as i16)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(username, level)| (username, AccessLevel::from(level)))
        .collect())
}

pub async fn revoke_repo_access(pool: &PgPool, user_id: i64, repo_id: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;
    if !lock_access_repository(&mut tx, repo_id).await? {
        return Ok(false);
    }
    let removed = revoke_repo_access_in_transaction(&mut tx, user_id, repo_id).await?;
    tx.commit().await?;
    Ok(removed)
}
