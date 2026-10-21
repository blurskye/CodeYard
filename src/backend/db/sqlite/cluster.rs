//! Cluster membership on SQLite. Roles are a JSON array in a TEXT column.
//! Clustering itself needs PostgreSQL; these keep the two engines interchangeable.
use super::models::{
    ClusterNode, HeldReplica, JoinInvite, JoinOutcome, NodeReplicaCounts, NodeStatusChange,
};
use super::time;
use crate::backend::db::cluster::{
    Authority, DeclaredMember, FamilyOutcome, JoinRequest, NewJoinToken, PrimaryReplicaRow,
    PromoteOutcome,
};
use crate::backend::db::sqlite::users::transitions::{active_admin, live_session};
use anyhow::{anyhow, bail, Result};
use sqlx::{SqliteConnection, SqlitePool};

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%f000Z','now')";

/// Turns the name-uniqueness failure into a message an operator can act on.
fn name_conflict(error: sqlx::Error, name: &str) -> anyhow::Error {
    match &error {
        sqlx::Error::Database(database) if database.is_unique_violation() => {
            anyhow!("another node already uses the name '{name}'")
        }
        _ => error.into(),
    }
}

pub async fn create_join_token(pool: &SqlitePool, token: NewJoinToken<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO join_tokens (token_hash, roles, reusable, requires_approval, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(token.token_hash)
    .bind(serde_json::to_string(token.roles)?)
    .bind(token.reusable)
    .bind(token.requires_approval)
    .bind(time::encode(token.expires_at))
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_join_tokens(pool: &SqlitePool) -> Result<Vec<JoinInvite>> {
    Ok(sqlx::query_as::<_, JoinInvite>(&format!(
        "SELECT id, roles, reusable, requires_approval, expires_at, uses, created_at
         FROM join_tokens WHERE expires_at > {NOW} AND (reusable OR uses = 0) ORDER BY id"
    ))
    .fetch_all(pool)
    .await?)
}

pub async fn delete_join_token(pool: &SqlitePool, id: i64) -> Result<bool> {
    Ok(sqlx::query("DELETE FROM join_tokens WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected()
        == 1)
}

pub async fn list_unplaced_repositories(pool: &SqlitePool) -> Result<Vec<(i64, String)>> {
    Ok(sqlx::query_as(
        "SELECT id, disk_path FROM repositories WHERE storage_node_id IS NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn place_repository(pool: &SqlitePool, repo_id: i64, node_id: i64) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE repositories SET storage_node_id = ?2 WHERE id = ?1 AND storage_node_id IS NULL",
    )
    .bind(repo_id)
    .bind(node_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn place_fork_family(
    pool: &SqlitePool,
    repo_id: i64,
    node_id: i64,
    local: &[i64],
) -> Result<Vec<i64>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let members: Vec<(i64, Option<i64>, i64)> = sqlx::query_as(FORK_FAMILY)
        .bind(repo_id)
        .fetch_all(&mut *tx)
        .await?;
    if members.is_empty()
        || members
            .iter()
            .any(|(id, node, _)| node.is_some() || !local.contains(id))
    {
        return Ok(Vec::new());
    }
    let ids: Vec<_> = members.into_iter().map(|row| row.0).collect();
    for id in &ids {
        sqlx::query("UPDATE repositories SET storage_node_id=?2 WHERE id=?1")
            .bind(id)
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(ids)
}

pub async fn join_with_token(pool: &SqlitePool, request: JoinRequest<'_>) -> Result<JoinOutcome> {
    // SQLite has a single writer, so BEGIN IMMEDIATE keeps two joins apart. Every
    // early return below drops the transaction, which rolls it back.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let known = sqlx::query_as::<_, ClusterNode>(&format!(
        "SELECT *, {NOW} AS observed_at FROM nodes WHERE public_key = ?1"
    ))
    .bind(request.public_key)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(node) = known {
        return Ok(JoinOutcome::AlreadyMember(node));
    }
    let token = sqlx::query_as::<_, (String, bool)>(&format!(
        "UPDATE join_tokens SET uses = uses + 1
         WHERE token_hash = ?1 AND expires_at > {NOW} AND (reusable OR uses = 0)
         RETURNING roles, requires_approval"
    ))
    .bind(request.token_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((allowed, requires_approval)) = token else {
        return Ok(JoinOutcome::InvalidToken);
    };
    let allowed: Vec<String> = serde_json::from_str(&allowed)?;
    let roles = match request.roles {
        None => allowed,
        Some(wanted) if wanted.iter().all(|role| allowed.contains(role)) => wanted.to_vec(),
        Some(_) => return Ok(JoinOutcome::RolesNotAllowed { allowed }),
    };
    let status = if requires_approval {
        "pending"
    } else {
        "active"
    };
    // A joined node is not live until it has published itself, so no last_seen here.
    let inserted = sqlx::query_as::<_, ClusterNode>(&format!(
        "INSERT INTO nodes (public_key, name, roles, status, version)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT DO NOTHING
         RETURNING *, {NOW} AS observed_at"
    ))
    .bind(request.public_key)
    .bind(request.name)
    .bind(serde_json::to_string(&roles)?)
    .bind(status)
    .bind(request.version)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(node) = inserted {
        tx.commit().await?;
        return Ok(JoinOutcome::Joined(node));
    }
    let raced = sqlx::query_as::<_, ClusterNode>(&format!(
        "SELECT *, {NOW} AS observed_at FROM nodes WHERE public_key = ?1"
    ))
    .bind(request.public_key)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(match raced {
        Some(node) => JoinOutcome::AlreadyMember(node),
        None => JoinOutcome::NameTaken,
    })
}

pub async fn register_own_node(
    pool: &SqlitePool,
    public_key: &str,
    name: &str,
    roles: &[String],
    version: &str,
) -> Result<ClusterNode> {
    sqlx::query_as::<_, ClusterNode>(&format!(
        "INSERT INTO nodes (public_key, name, roles, status, version)
         VALUES (?1, ?2, ?3, 'active', ?4)
         ON CONFLICT (public_key) DO UPDATE SET version = excluded.version
         RETURNING *, {NOW} AS observed_at"
    ))
    .bind(public_key)
    .bind(name)
    .bind(serde_json::to_string(roles)?)
    .bind(version)
    .fetch_one(pool)
    .await
    .map_err(|error| name_conflict(error, name))
}

pub async fn upsert_declared_nodes(
    pool: &SqlitePool,
    nodes: &[DeclaredMember],
) -> Result<Vec<ClusterNode>> {
    // BEGIN IMMEDIATE: one writer at a time. Every early return drops the transaction.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let mut rows = Vec::with_capacity(nodes.len());
    for node in nodes {
        let row = sqlx::query_as::<_, ClusterNode>(&format!(
            "INSERT INTO nodes (public_key, name, roles, status)
             VALUES (?1, ?2, ?3, 'active')
             ON CONFLICT (public_key) DO UPDATE SET
                 name = excluded.name,
                 roles = excluded.roles,
                 status = CASE WHEN nodes.status = 'revoked' THEN 'revoked' ELSE 'active' END
             RETURNING *, {NOW} AS observed_at"
        ))
        .bind(node.public_key.as_str())
        .bind(node.name.as_str())
        .bind(serde_json::to_string(&node.roles)?)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| name_conflict(error, &node.name))?;
        rows.push(row);
    }
    tx.commit().await?;
    Ok(rows)
}

pub async fn get_node_by_public_key(
    pool: &SqlitePool,
    public_key: &str,
) -> Result<Option<ClusterNode>> {
    Ok(sqlx::query_as::<_, ClusterNode>(&format!(
        "SELECT *, {NOW} AS observed_at FROM nodes WHERE public_key = ?1"
    ))
    .bind(public_key)
    .fetch_optional(pool)
    .await?)
}

pub async fn get_node_by_name(pool: &SqlitePool, name: &str) -> Result<Option<ClusterNode>> {
    Ok(sqlx::query_as::<_, ClusterNode>(&format!(
        "SELECT *, {NOW} AS observed_at FROM nodes WHERE name = ?1"
    ))
    .bind(name)
    .fetch_optional(pool)
    .await?)
}

pub async fn list_nodes(pool: &SqlitePool) -> Result<Vec<ClusterNode>> {
    Ok(sqlx::query_as::<_, ClusterNode>(&format!(
        "SELECT *, {NOW} AS observed_at FROM nodes ORDER BY name"
    ))
    .fetch_all(pool)
    .await?)
}

pub async fn is_active_node(pool: &SqlitePool, public_key: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM nodes WHERE public_key = ?1 AND status = 'active')",
    )
    .bind(public_key)
    .fetch_one(pool)
    .await?)
}

pub async fn set_node_status(
    pool: &SqlitePool,
    authority: Authority<'_>,
    name: &str,
    status: &str,
) -> Result<NodeStatusChange> {
    // BEGIN IMMEDIATE: the administrator check and the write see the same state.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if let Authority::Administrator { user_id, session } = authority {
        if !active_admin(&mut tx, user_id).await?
            || !live_session(&mut tx, user_id, session).await?
        {
            return Ok(NodeStatusChange::Denied);
        }
    }
    let before: Option<String> = sqlx::query_scalar("SELECT status FROM nodes WHERE name = ?1")
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(before) = before else {
        return Ok(NodeStatusChange::Missing);
    };
    let node = sqlx::query_as::<_, ClusterNode>(&format!(
        "UPDATE nodes SET status = ?2 WHERE name = ?1 RETURNING *, {NOW} AS observed_at"
    ))
    .bind(name)
    .bind(status)
    .fetch_one(&mut *tx)
    .await?;
    if let Authority::Administrator { user_id: actor, .. } = authority {
        sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES(?1,'node.status','node',?2,?3)")
            .bind(actor).bind(node.id).bind(serde_json::json!({"before":before,"status":status}).to_string()).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(NodeStatusChange::Changed {
        before,
        node: Box::new(node),
    })
}

pub async fn record_node_heartbeat(
    pool: &SqlitePool,
    public_key: &str,
    version: &str,
    addr_hint: &str,
    disk_free: Option<i64>,
) -> Result<bool> {
    let result = sqlx::query(&format!(
        "UPDATE nodes SET last_seen = {NOW}, version = ?2, addr_hint = ?3, disk_free = ?4 WHERE public_key = ?1"
    ))
    .bind(public_key)
    .bind(version)
    .bind(addr_hint)
    .bind(disk_free)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn touch_node(pool: &SqlitePool, public_key: &str) -> Result<()> {
    sqlx::query(&format!(
        "UPDATE nodes SET last_seen = {NOW} WHERE public_key = ?1 AND status = 'active'"
    ))
    .bind(public_key)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn clear_node_heartbeat(pool: &SqlitePool, public_key: &str) -> Result<()> {
    sqlx::query("UPDATE nodes SET last_seen = NULL WHERE public_key = ?1")
        .bind(public_key)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_node_by_id(pool: &SqlitePool, id: i64) -> Result<Option<ClusterNode>> {
    Ok(sqlx::query_as(&format!(
        "SELECT *, {NOW} AS observed_at FROM nodes WHERE id = ?1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn get_repository_by_uuid(
    pool: &SqlitePool,
    uuid: &str,
) -> Result<Option<crate::backend::db::models::Repository>> {
    Ok(
        sqlx::query_as("SELECT * FROM repositories WHERE substr(disk_path, -40) = ?1 || '.git'")
            .bind(uuid)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn get_replicas(pool: &SqlitePool) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT replicas FROM node_configuration WHERE id = 1")
            .fetch_optional(pool)
            .await?
            .unwrap_or_else(|| "1".to_owned()),
    )
}

pub async fn set_replicas(pool: &SqlitePool, value: &str) -> Result<bool> {
    Ok(sqlx::query(&format!(
        "UPDATE node_configuration SET replicas = ?1, updated_at = {NOW} WHERE id = 1"
    ))
    .bind(value)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn bump_write_version(
    pool: &SqlitePool,
    repo_id: i64,
    node_id: Option<Option<i64>>,
) -> Result<bool> {
    // BEGIN IMMEDIATE: the read and the bump see the state a promotion sees, as in
    // `promote_repository`.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let held_by: Option<Option<i64>> =
        sqlx::query_scalar("SELECT storage_node_id FROM repositories WHERE id = ?1")
            .bind(repo_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(held_by) = held_by else {
        bail!("repository {repo_id} not found");
    };
    if node_id.is_some_and(|expected| held_by != expected) {
        return Ok(false);
    }
    // A repository that no storage node holds has no replicas, so its changes are not counted.
    if held_by.is_some() {
        sqlx::query("UPDATE repositories SET write_version = write_version + 1 WHERE id = ?1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn get_write_version(pool: &SqlitePool, repo_id: i64) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT write_version FROM repositories WHERE id = ?1")
            .bind(repo_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn list_primary_repositories(
    pool: &SqlitePool,
    node_id: i64,
) -> Result<Vec<PrimaryReplicaRow>> {
    Ok(sqlx::query_as(
        "SELECT r.id, r.disk_path, rr.node_id, rr.applied_version, r.size_bytes
         FROM repositories r
         LEFT JOIN repository_replicas rr ON rr.repository_id = r.id
         WHERE r.storage_node_id = ?1
         ORDER BY r.id, rr.node_id",
    )
    .bind(node_id)
    .fetch_all(pool)
    .await?)
}

pub async fn pending_replica_bytes(pool: &SqlitePool) -> Result<Vec<(i64, i64)>> {
    Ok(sqlx::query_as("SELECT rr.node_id, SUM(MAX(r.size_bytes,1)) FROM repository_replicas rr JOIN repositories r ON r.id=rr.repository_id WHERE rr.applied_version < 0 GROUP BY rr.node_id")
        .fetch_all(pool).await?)
}

pub async fn list_held_replicas(pool: &SqlitePool, node_id: i64) -> Result<Vec<HeldReplica>> {
    Ok(sqlx::query_as(
        "SELECT rr.repository_id, r.disk_path, r.storage_node_id AS primary_node_id,
                rr.applied_version, r.write_version
         FROM repository_replicas rr
         JOIN repositories r ON r.id = rr.repository_id
         WHERE rr.node_id = ?1
         ORDER BY rr.repository_id",
    )
    .bind(node_id)
    .fetch_all(pool)
    .await?)
}

pub async fn list_replicas(pool: &SqlitePool, repo_id: i64) -> Result<Vec<(i64, i64)>> {
    Ok(sqlx::query_as(
        "SELECT node_id, applied_version FROM repository_replicas
         WHERE repository_id = ?1 ORDER BY node_id",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}

/// The fork family of the repository bound as `?1`. It walks forks in both directions: each
/// member adds its forks and its source. `UNION` stops the walk at repositories it has found.
const FORK_FAMILY: &str = "WITH RECURSIVE family(id) AS (
         SELECT ?1
         UNION
         SELECT r.id FROM family f
         JOIN repositories p ON p.id = f.id
         JOIN repositories r ON r.forked_from_id = f.id OR r.id = p.forked_from_id
     )
     SELECT r.id, r.storage_node_id, r.write_version
     FROM repositories r JOIN family f ON f.id = r.id
     ORDER BY r.id";

pub async fn list_fork_family(
    pool: &SqlitePool,
    repo_id: i64,
) -> Result<Vec<(i64, Option<i64>, i64)>> {
    Ok(sqlx::query_as(FORK_FAMILY)
        .bind(repo_id)
        .fetch_all(pool)
        .await?)
}

pub async fn insert_replica(
    pool: &SqlitePool,
    repo_id: i64,
    node_id: i64,
    primary_id: i64,
) -> Result<bool> {
    Ok(sqlx::query(
        "INSERT INTO repository_replicas (repository_id, node_id)
         SELECT r.id, ?2 FROM repositories r
         WHERE r.id = ?1 AND r.storage_node_id = ?3 AND ?2 <> ?3
         ON CONFLICT DO NOTHING",
    )
    .bind(repo_id)
    .bind(node_id)
    .bind(primary_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn delete_replica(
    pool: &SqlitePool,
    repo_id: i64,
    node_id: i64,
    primary_id: i64,
) -> Result<bool> {
    Ok(sqlx::query(
        "DELETE FROM repository_replicas WHERE repository_id = ?1 AND node_id = ?2
           AND EXISTS (SELECT 1 FROM repositories r
                       WHERE r.id = ?1 AND r.storage_node_id = ?3)",
    )
    .bind(repo_id)
    .bind(node_id)
    .bind(primary_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn set_applied_version(
    pool: &SqlitePool,
    repo_id: i64,
    node_id: i64,
    primary_id: i64,
    version: i64,
) -> Result<bool> {
    Ok(sqlx::query(&format!(
        "UPDATE repository_replicas SET applied_version = ?4, synced_at = {NOW}
         WHERE repository_id = ?1 AND node_id = ?2
           AND EXISTS (SELECT 1 FROM repositories r
                       WHERE r.id = ?1 AND r.storage_node_id = ?3)"
    ))
    .bind(repo_id)
    .bind(node_id)
    .bind(primary_id)
    .bind(version)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn replica_counts(pool: &SqlitePool) -> Result<Vec<NodeReplicaCounts>> {
    Ok(sqlx::query_as(
        "SELECT n.id AS node_id,
            (SELECT COUNT(*) FROM repositories r WHERE r.storage_node_id = n.id) AS primaries,
            (SELECT COUNT(*) FROM repository_replicas rr WHERE rr.node_id = n.id) AS replicas,
            (SELECT COUNT(*) FROM repository_replicas rr
                JOIN repositories r ON r.id = rr.repository_id
                WHERE rr.node_id = n.id AND rr.applied_version = r.write_version) AS current
         FROM nodes n ORDER BY n.name",
    )
    .fetch_all(pool)
    .await?)
}

/// The node as the database sees it now, with its heartbeat read against the database clock,
/// which is the clock that records it.
async fn node_at_db_time(conn: &mut SqliteConnection, node: i64) -> Result<Option<ClusterNode>> {
    Ok(sqlx::query_as::<_, ClusterNode>(&format!(
        "SELECT *, {NOW} AS observed_at FROM nodes WHERE id = ?1"
    ))
    .bind(node)
    .fetch_optional(&mut *conn)
    .await?)
}

pub async fn promote_repository(
    pool: &SqlitePool,
    repo_id: i64,
    from: i64,
    to: i64,
    require_current: bool,
) -> Result<PromoteOutcome> {
    // BEGIN IMMEDIATE: one writer at a time, so the checks and the change see the same
    // state. Every early return drops the transaction, which rolls it back.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let Some((primary, write_version)) = sqlx::query_as::<_, (Option<i64>, i64)>(
        "SELECT storage_node_id, write_version FROM repositories WHERE id = ?1",
    )
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(PromoteOutcome::Moved);
    };
    if primary != Some(from) {
        return Ok(PromoteOutcome::Moved);
    }
    let Some(applied_version) = sqlx::query_scalar::<_, i64>(
        "SELECT applied_version FROM repository_replicas WHERE repository_id = ?1 AND node_id = ?2",
    )
    .bind(repo_id)
    .bind(to)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(PromoteOutcome::NoReplica);
    };
    if applied_version < 0 || (require_current && applied_version != write_version) {
        return Ok(PromoteOutcome::Stale {
            applied_version,
            write_version,
        });
    }
    // A repository moved to a node that is down would have no primary to serve it.
    let target = node_at_db_time(&mut tx, to).await?;
    if !target.is_some_and(|node| node.status == "active" && node.is_online()) {
        return Ok(PromoteOutcome::TargetOffline);
    }
    move_primary(&mut tx, repo_id, from, to, applied_version).await?;
    tx.commit().await?;
    Ok(PromoteOutcome::Promoted {
        applied_version,
        write_version,
    })
}

/// Moves the whole fork family of `repo_id` from `from` to `to` in one transaction, or not at
/// all. The family is read and its rows checked inside the write transaction, so a member that
/// another node has already moved is refused, and two take-overs cannot split a family.
pub async fn promote_fork_family(
    pool: &SqlitePool,
    repo_id: i64,
    from: i64,
    to: i64,
) -> Result<FamilyOutcome> {
    // BEGIN IMMEDIATE: one writer at a time, so the family read and the moves see the same
    // state. Every early return drops the transaction, which rolls it back.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let members: Vec<(i64, Option<i64>, i64)> = sqlx::query_as(FORK_FAMILY)
        .bind(repo_id)
        .fetch_all(&mut *tx)
        .await?;
    if members.is_empty() || members.iter().any(|(_, primary, _)| *primary != Some(from)) {
        return Ok(FamilyOutcome::Refused);
    }
    let primary = node_at_db_time(&mut tx, from).await?;
    if primary.is_none_or(|node| node.status != "active" || node.is_online()) {
        return Ok(FamilyOutcome::Refused);
    }
    // The taker must be reporting in too, or the family would move onto a node that is down.
    let target = node_at_db_time(&mut tx, to).await?;
    if !target.is_some_and(|node| node.status == "active" && node.is_online()) {
        return Ok(FamilyOutcome::Refused);
    }
    for &(member, _, write_version) in &members {
        let applied = sqlx::query_scalar::<_, i64>(
            "SELECT applied_version FROM repository_replicas
             WHERE repository_id = ?1 AND node_id = ?2",
        )
        .bind(member)
        .bind(to)
        .fetch_optional(&mut *tx)
        .await?;
        if applied != Some(write_version) {
            return Ok(FamilyOutcome::Refused);
        }
    }
    for &(member, _, write_version) in &members {
        move_primary(&mut tx, member, from, to, write_version).await?;
    }
    tx.commit().await?;
    Ok(FamilyOutcome::Promoted(
        members.into_iter().map(|(id, _, _)| id).collect(),
    ))
}

/// Makes `to` the primary of `repo_id` inside the caller's transaction. `to` holds
/// `applied_version` writes, and copies ahead of that have to copy again. `from` keeps a copy
/// with nothing applied, which catches up when its node is back.
async fn move_primary(
    conn: &mut SqliteConnection,
    repo_id: i64,
    from: i64,
    to: i64,
    applied_version: i64,
) -> Result<()> {
    // A forced rollback must not reuse the version of an earlier primary's history.
    sqlx::query("UPDATE repositories SET storage_node_id = ?2, write_version = write_version + CASE WHEN ?3 < write_version THEN 1 ELSE 0 END WHERE id = ?1")
        .bind(repo_id)
        .bind(to)
        .bind(applied_version)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE repository_replicas SET applied_version = MIN(applied_version, ?3)
         WHERE repository_id = ?1 AND node_id <> ?2",
    )
    .bind(repo_id)
    .bind(to)
    .bind(applied_version)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM repository_replicas WHERE repository_id = ?1 AND node_id = ?2")
        .bind(repo_id)
        .bind(to)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO repository_replicas (repository_id, node_id) VALUES (?1, ?2)
         ON CONFLICT DO NOTHING",
    )
    .bind(repo_id)
    .bind(from)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
