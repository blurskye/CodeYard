use crate::repositories::access::CollaboratorChange;
use anyhow::Result;
use codeyard_db_core::models::AccessLevel;
use codeyard_db_core::models::RepoAccess;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::User;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;

pub async fn git_access(
    pool: &SqlitePool,
    user_id: i64,
    repo_id: i64,
    write: bool,
) -> Result<AccessLevel> {
    let level: Option<i16> = sqlx::query_scalar(&format!("SELECT CASE WHEN u.is_banned OR u.must_change_password OR ($3 AND r.is_archived) THEN 0 WHEN r.is_gist THEN CASE WHEN EXISTS(SELECT 1 FROM gists g JOIN users own ON own.id=r.owner_user_id WHERE g.repo_id=r.id AND g.ready AND NOT own.is_banned) THEN CASE WHEN r.owner_user_id=u.id THEN 6 WHEN NOT $3 THEN 1 ELSE 0 END ELSE 0 END WHEN r.owner_kind='user' AND u.is_admin THEN 6 ELSE ({USER_REPO_ACCESS_SQL}) END FROM repositories r JOIN users u ON u.id=$1 WHERE r.id=$2"))
        .bind(user_id).bind(repo_id).bind(write).fetch_optional(pool).await?;
    Ok(AccessLevel::from(level.unwrap_or(0)))
}

pub async fn author_associations(
    pool: &SqlitePool,
    repo_id: i64,
    users: &[i64],
) -> Result<Vec<(i64, String)>> {
    anyhow::ensure!(
        users.len() <= 500,
        "Author association batch exceeds its limit"
    );
    if users.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(&format!("SELECT u.id,CASE WHEN r.owner_kind='user' AND r.owner_user_id=u.id THEN 'OWNER' WHEN NOT u.is_banned AND NOT u.must_change_password AND EXISTS(SELECT 1 FROM org_memberships m JOIN organizations o ON o.id=m.org_id WHERE m.org_id=r.owner_org_id AND m.user_id=u.id AND m.role IN ('owner','admin','member')) THEN 'MEMBER' WHEN ({USER_REPO_ACCESS_SQL}) >= CASE WHEN EXISTS(SELECT 1 FROM repo_access a WHERE a.repo_id=r.id AND a.user_id=u.id AND a.access_level>0) THEN 1 ELSE 3 END THEN 'COLLABORATOR' ELSE 'NONE' END FROM repositories r CROSS JOIN users u WHERE r.id=?1 AND u.id IN (SELECT value FROM json_each(?2))"))
        .bind(repo_id).bind(serde_json::to_string(users)?).fetch_all(pool).await?)
}

pub async fn get_user_repo_access(
    pool: &SqlitePool,
    user_id: i64,
    repo_id: i64,
) -> Result<AccessLevel> {
    let mut connection = pool.acquire().await?;
    user_repo_access_in_transaction(&mut connection, user_id, repo_id).await
}

pub async fn get_users_repo_access(
    pool: &SqlitePool,
    user_ids: &[i64],
    repo_id: i64,
) -> Result<Vec<(i64, AccessLevel)>> {
    if user_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<Sqlite>::new(format!("SELECT u.id, ({USER_REPO_ACCESS_SQL}) FROM repositories r CROSS JOIN users u WHERE r.id = "));
    query.push_bind(repo_id).push(" AND u.id IN (");
    let mut ids = query.separated(",");
    for id in user_ids {
        ids.push_bind(id);
    }
    query.push(")");
    let rows: Vec<(i64, i16)> = query.build_query_as().fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(id, level)| (id, AccessLevel::from(level)))
        .collect())
}

pub async fn readable_repository_ids(
    pool: &SqlitePool,
    user_id: Option<i64>,
    repo_ids: &[i64],
) -> Result<Vec<i64>> {
    anyhow::ensure!(repo_ids.len() <= 100, "Repository page exceeds its limit");
    Ok(sqlx::query_scalar(&format!("SELECT r.id FROM repositories r LEFT JOIN users u ON u.id=?1 WHERE r.id IN (SELECT value FROM json_each(?2)) AND NOT r.is_gist AND (NOT r.is_private OR (r.owner_kind='user' AND NOT u.is_banned AND NOT u.must_change_password AND (u.is_admin OR r.owner_user_id=u.id)) OR ({USER_REPO_ACCESS_SQL}) >= 1)"))
        .bind(user_id).bind(serde_json::to_string(repo_ids)?).fetch_all(pool).await?)
}

const PUBLIC_READ_BASELINE: &str = "CASE WHEN NOT r.is_private THEN 1 ELSE 0 END";

/// The access projection without the read that every visitor has on a public repository: what
/// the user's own grants, organization settings and team memberships give. A collaborator is
/// someone with such a grant, not everyone who can read.
pub(crate) fn user_repo_grant_sql() -> String {
    debug_assert!(USER_REPO_ACCESS_SQL.contains(PUBLIC_READ_BASELINE));
    USER_REPO_ACCESS_SQL.replacen(PUBLIC_READ_BASELINE, "0", 1)
}

/// Current visibility and metadata in one snapshot for a bounded run list.
pub async fn readable_repositories_by_ids(
    pool: &SqlitePool,
    user_id: i64,
    ids: &[i64],
) -> Result<Vec<Repository>> {
    debug_assert!(ids.len() <= 500);
    Ok(sqlx::query_as(&format!(
        "SELECT r.* FROM repositories r JOIN users u ON u.id = ?1 WHERE r.id IN (SELECT value FROM json_each(?2)) AND NOT u.is_banned AND NOT u.must_change_password AND {}",
        readable_condition()
    ))
    .bind(user_id)
    .bind(serde_json::to_string(ids)?)
    .fetch_all(pool)
    .await?)
}

pub(super) async fn grant_repo_access_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    user_id: i64,
    repo_id: i64,
    level: AccessLevel,
) -> Result<RepoAccess> {
    anyhow::ensure!(
        !codeyard_db_core::queries::sqlite::users::blocks::repository_interaction_blocked(
            tx, user_id, repo_id
        )
        .await?,
        codeyard_db_identity::users::blocks::InteractionBlocked
    );
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
    sqlx::query("UPDATE repository_invitations SET state='cancelled' WHERE repo_id=?1 AND invitee_id=?2 AND state='pending'").bind(repo_id).bind(user_id).execute(&mut **tx).await?;
    if previous.is_some_and(|previous| previous > level as i16) {
        codeyard_db_core::queries::sqlite::ci::stop_actor_repo_ci_runs(tx, user_id, repo_id)
            .await?;
    }
    Ok(result)
}

pub(super) async fn revoke_repo_access_in_transaction(
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
    sqlx::query("UPDATE repository_invitations SET state='cancelled' WHERE repo_id=?1 AND invitee_id=?2 AND state='pending'").bind(repo_id).bind(user_id).execute(&mut **tx).await?;
    if removed {
        // The collaborator's queued runs here are cancelled and running ones are
        // asked to stop, like the organization revocations do.
        codeyard_db_core::queries::sqlite::ci::stop_actor_repo_ci_runs(tx, user_id, repo_id)
            .await?;
    }
    Ok(removed)
}

pub async fn change_personal_repository_collaborator(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
    username: &str,
    permission: Option<AccessLevel>,
) -> Result<CollaboratorChange> {
    let admission = actor.into();
    let actor = admission.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(credential) = admission.credential() {
        codeyard_db_identity::sqlite::users::transitions::require_account(
            &mut tx,
            actor,
            &credential,
        )
        .await?;
    }
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
    if permission.is_some()
        && codeyard_db_identity::sqlite::users::blocks::interaction_blocked(
            &mut tx, actor, target.id,
        )
        .await?
    {
        return Ok(CollaboratorChange::NotFound);
    }
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

/// Current writers among a supplied set, using the same access projection as individual checks.
pub async fn write_access_user_ids(
    pool: &SqlitePool,
    repo_id: i64,
    ids: &[i64],
) -> Result<Vec<i64>> {
    let mut connection = pool.acquire().await?;
    write_access_user_ids_on(&mut connection, repo_id, ids).await
}

pub(super) async fn write_access_user_ids_on(
    connection: &mut sqlx::SqliteConnection,
    repo_id: i64,
    ids: &[i64],
) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT u.id FROM repositories r JOIN users u ON u.id IN (SELECT value FROM json_each(?2))
         WHERE r.id = ?1 AND NOT r.is_archived AND NOT u.is_banned AND NOT u.must_change_password
         AND ((r.owner_kind = 'user' AND (u.is_admin OR r.owner_user_id = u.id)) OR ({USER_REPO_ACCESS_SQL}) >= ?3)"
    )).bind(repo_id).bind(serde_json::to_string(ids)?).bind(AccessLevel::Write as i16).fetch_all(connection).await?)
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

use codeyard_db_core::queries::sqlite::repositories::access::USER_REPO_ACCESS_SQL;

use codeyard_db_core::queries::sqlite::repositories::access::readable_condition;

use codeyard_db_core::queries::sqlite::repositories::access::user_repo_access_in_transaction;
