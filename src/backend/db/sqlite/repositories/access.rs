use crate::backend::{
    db::models::{AccessLevel, RepoAccess, User},
    repositories::access::{CollaboratorAccessTransaction, CollaboratorChange},
};
use anyhow::Result;
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

pub async fn get_user_repo_access(
    pool: &SqlitePool,
    user_id: i64,
    repo_id: i64,
) -> Result<AccessLevel> {
    let mut connection = pool.acquire().await?;
    user_repo_access_in_transaction(&mut connection, user_id, repo_id).await
}

/// Effectful callers use BEGIN IMMEDIATE and recheck before durable admission.
/// A pool projection alone is never a write permit.
pub(crate) async fn user_repo_access_in_transaction(
    connection: &mut SqliteConnection,
    user_id: i64,
    repo_id: i64,
) -> Result<AccessLevel> {
    // Projection only, not a write permit. All contributions and governance
    // share one statement snapshot; suspended orgs cap even nonmember grants.
    // Collaborator levels are 1..=5; Owner comes only from scoped ownership.
    let level: Option<i16> = sqlx::query_scalar(&format!(
        "SELECT ({USER_REPO_ACCESS_SQL}) FROM repositories r JOIN users u ON u.id = ?1 WHERE r.id = ?2"
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
        THEN MIN(MAX(
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
                            AND t.parent_team_id IS NULL
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
        ELSE 0 END)
    "#;

async fn grant_repo_access_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    user_id: i64,
    repo_id: i64,
    level: AccessLevel,
) -> Result<RepoAccess> {
    let previous: Option<i16> = sqlx::query_scalar(
        "SELECT access_level FROM repo_access WHERE user_id = ?1 AND repo_id = ?2",
    )
    .bind(user_id)
    .bind(repo_id)
    .fetch_optional(&mut **tx)
    .await?;
    let result = sqlx::query_as::<_, RepoAccess>(
        r#"
        INSERT INTO repo_access (user_id, repo_id, access_level)
        VALUES (?1, ?2, ?3)
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
    tx: &mut Transaction<'_, Sqlite>,
    user_id: i64,
    repo_id: i64,
) -> Result<bool> {
    let result = sqlx::query("DELETE FROM repo_access WHERE user_id = ?1 AND repo_id = ?2")
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

impl CollaboratorAccessTransaction for SqlitePool {
    async fn change_personal_repository_collaborator(
        &self,
        actor: i64,
        repo_id: i64,
        username: &str,
        permission: Option<AccessLevel>,
    ) -> Result<CollaboratorChange> {
        let mut tx = self.begin_with("BEGIN IMMEDIATE").await?;
        let actor_state: Option<(bool, bool)> =
            sqlx::query_as("SELECT is_banned,must_change_password FROM users WHERE id=?1")
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
        let target = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username=?1")
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
            "SELECT id FROM repositories WHERE id=?1 AND owner_kind='user' AND owner_user_id=?2 AND owner_org_id IS NULL",
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
}

pub async fn grant_repo_access(
    pool: &SqlitePool,
    user_id: i64,
    repo_id: i64,
    level: AccessLevel,
) -> Result<RepoAccess> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let result = grant_repo_access_in_transaction(&mut tx, user_id, repo_id, level).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn list_repo_collaborators(pool: &SqlitePool, repo_id: i64) -> Result<Vec<RepoAccess>> {
    let results = sqlx::query_as::<_, RepoAccess>(
        "SELECT * FROM repo_access WHERE repo_id = ?1 ORDER BY access_level DESC",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;

    Ok(results)
}

/// Up to 30 users other than `viewer_id` who can write to the repository, most privileged first.
/// The candidates are the owner, the collaborators and the organization members. Each level is
/// the effective access from `USER_REPO_ACCESS_SQL`, so teams and banned accounts count as they do
/// for writes.
pub async fn list_write_access_users(
    pool: &SqlitePool,
    repo_id: i64,
    viewer_id: i64,
) -> Result<Vec<(String, AccessLevel)>> {
    let rows: Vec<(String, i16)> = sqlx::query_as(&format!(
        "SELECT username, level FROM (
            SELECT u.username, ({USER_REPO_ACCESS_SQL}) AS level
            FROM repositories r JOIN users u ON u.id IN (
                SELECT owner_user_id FROM repositories WHERE id = ?1 AND owner_user_id IS NOT NULL
                UNION SELECT user_id FROM repo_access WHERE repo_id = ?1
                UNION SELECT m.user_id FROM repositories o JOIN org_memberships m ON m.org_id = o.owner_org_id
                    WHERE o.id = ?1 AND o.owner_user_id IS NULL
            )
            WHERE r.id = ?1 AND u.id <> ?2
        ) candidates
        WHERE level >= ?3 ORDER BY level DESC, username LIMIT 30"
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

pub async fn revoke_repo_access(pool: &SqlitePool, user_id: i64, repo_id: i64) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let removed = revoke_repo_access_in_transaction(&mut tx, user_id, repo_id).await?;
    tx.commit().await?;
    Ok(removed)
}
