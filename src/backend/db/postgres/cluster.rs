//! Cluster membership on PostgreSQL. Roles are a `TEXT[]` column.
use super::models::{
    ClusterNode, HeldReplica, JoinInvite, JoinOutcome, NodeReplicaCounts, NodeStatusChange,
};
use crate::backend::db::cluster::{
    Authority, DeclaredMember, FamilyOutcome, JoinRequest, NewJoinToken, PromoteOutcome,
};
use crate::backend::db::postgres::users::transitions::active_admin;
use anyhow::{anyhow, bail, Result};
use sqlx::{PgConnection, PgPool};

/// Turns the name-uniqueness failure into a message an operator can act on.
fn name_conflict(error: sqlx::Error, name: &str) -> anyhow::Error {
    match &error {
        sqlx::Error::Database(database) if database.is_unique_violation() => {
            anyhow!("another node already uses the name '{name}'")
        }
        _ => error.into(),
    }
}

pub async fn create_join_token(pool: &PgPool, token: NewJoinToken<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO join_tokens (token_hash, roles, reusable, requires_approval, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(token.token_hash)
    .bind(token.roles.to_vec())
    .bind(token.reusable)
    .bind(token.requires_approval)
    .bind(token.expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_join_tokens(pool: &PgPool) -> Result<Vec<JoinInvite>> {
    Ok(sqlx::query_as::<_, JoinInvite>(
        "SELECT id, roles, reusable, requires_approval, expires_at, uses, created_at
         FROM join_tokens WHERE expires_at > NOW() AND (reusable OR uses = 0) ORDER BY id",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn delete_join_token(pool: &PgPool, id: i64) -> Result<bool> {
    Ok(sqlx::query("DELETE FROM join_tokens WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected()
        == 1)
}

pub async fn list_unplaced_repositories(pool: &PgPool) -> Result<Vec<(i64, String)>> {
    Ok(sqlx::query_as(
        "SELECT id, disk_path FROM repositories WHERE storage_node_id IS NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn place_repository(pool: &PgPool, repo_id: i64, node_id: i64) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE repositories SET storage_node_id = $2 WHERE id = $1 AND storage_node_id IS NULL",
    )
    .bind(repo_id)
    .bind(node_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn join_with_token(pool: &PgPool, request: JoinRequest<'_>) -> Result<JoinOutcome> {
    // Every early return below drops the transaction, which rolls it back.
    let mut tx = pool.begin().await?;
    // A key that is registered already gets its standing back and keeps the token unused.
    let known = sqlx::query_as::<_, ClusterNode>(
        "SELECT *, NOW() AS observed_at FROM nodes WHERE public_key = $1",
    )
    .bind(request.public_key)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(node) = known {
        return Ok(JoinOutcome::AlreadyMember(node));
    }
    // One statement checks expiry and use count and takes the use, so two joins
    // cannot both succeed with a one-time token.
    let token = sqlx::query_as::<_, (Vec<String>, bool)>(
        "UPDATE join_tokens SET uses = uses + 1
         WHERE token_hash = $1 AND expires_at > NOW() AND (reusable OR uses = 0)
         RETURNING roles, requires_approval",
    )
    .bind(request.token_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((allowed, requires_approval)) = token else {
        return Ok(JoinOutcome::InvalidToken);
    };
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
    let inserted = sqlx::query_as::<_, ClusterNode>(
        "INSERT INTO nodes (public_key, name, roles, status, version)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT DO NOTHING
         RETURNING *, NOW() AS observed_at",
    )
    .bind(request.public_key)
    .bind(request.name)
    .bind(roles)
    .bind(status)
    .bind(request.version)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(node) = inserted {
        tx.commit().await?;
        return Ok(JoinOutcome::Joined(node));
    }
    // Either the name is taken, or the same key joined concurrently.
    let raced = sqlx::query_as::<_, ClusterNode>(
        "SELECT *, NOW() AS observed_at FROM nodes WHERE public_key = $1",
    )
    .bind(request.public_key)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(match raced {
        Some(node) => JoinOutcome::AlreadyMember(node),
        None => JoinOutcome::NameTaken,
    })
}

pub async fn register_own_node(
    pool: &PgPool,
    public_key: &str,
    name: &str,
    roles: &[String],
    version: &str,
) -> Result<ClusterNode> {
    sqlx::query_as::<_, ClusterNode>(
        "INSERT INTO nodes (public_key, name, roles, status, version)
         VALUES ($1, $2, $3, 'active', $4)
         ON CONFLICT (public_key) DO UPDATE SET version = EXCLUDED.version
         RETURNING *, NOW() AS observed_at",
    )
    .bind(public_key)
    .bind(name)
    .bind(roles.to_vec())
    .bind(version)
    .fetch_one(pool)
    .await
    .map_err(|error| name_conflict(error, name))
}

pub async fn upsert_declared_nodes(
    pool: &PgPool,
    nodes: &[DeclaredMember],
) -> Result<Vec<ClusterNode>> {
    // Every early return drops the transaction, which rolls back the entries before it.
    let mut tx = pool.begin().await?;
    let mut rows = Vec::with_capacity(nodes.len());
    for node in nodes {
        let row = sqlx::query_as::<_, ClusterNode>(
            "INSERT INTO nodes (public_key, name, roles, status)
             VALUES ($1, $2, $3, 'active')
             ON CONFLICT (public_key) DO UPDATE SET
                 name = EXCLUDED.name,
                 roles = EXCLUDED.roles,
                 status = CASE WHEN nodes.status = 'revoked' THEN 'revoked' ELSE 'active' END
             RETURNING *, NOW() AS observed_at",
        )
        .bind(node.public_key.as_str())
        .bind(node.name.as_str())
        .bind(node.roles.clone())
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| name_conflict(error, &node.name))?;
        rows.push(row);
    }
    tx.commit().await?;
    Ok(rows)
}

pub async fn get_node_by_public_key(
    pool: &PgPool,
    public_key: &str,
) -> Result<Option<ClusterNode>> {
    Ok(sqlx::query_as::<_, ClusterNode>(
        "SELECT *, NOW() AS observed_at FROM nodes WHERE public_key = $1",
    )
    .bind(public_key)
    .fetch_optional(pool)
    .await?)
}

pub async fn get_node_by_name(pool: &PgPool, name: &str) -> Result<Option<ClusterNode>> {
    Ok(sqlx::query_as::<_, ClusterNode>(
        "SELECT *, NOW() AS observed_at FROM nodes WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?)
}

pub async fn list_nodes(pool: &PgPool) -> Result<Vec<ClusterNode>> {
    Ok(
        sqlx::query_as::<_, ClusterNode>("SELECT *, NOW() AS observed_at FROM nodes ORDER BY name")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn is_active_node(pool: &PgPool, public_key: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM nodes WHERE public_key = $1 AND status = 'active')",
    )
    .bind(public_key)
    .fetch_one(pool)
    .await?)
}

pub async fn set_node_status(
    pool: &PgPool,
    authority: Authority,
    name: &str,
    status: &str,
) -> Result<NodeStatusChange> {
    let mut tx = pool.begin().await?;
    if let Authority::Administrator(actor) = authority {
        // This locks the administrator's row until commit, so a concurrent ban either
        // waits for this change or lands before it.
        if !active_admin(&mut tx, actor).await? {
            return Ok(NodeStatusChange::Denied);
        }
    }
    let before: Option<String> =
        sqlx::query_scalar("SELECT status FROM nodes WHERE name = $1 FOR UPDATE")
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(before) = before else {
        return Ok(NodeStatusChange::Missing);
    };
    let node = sqlx::query_as::<_, ClusterNode>(
        "UPDATE nodes SET status = $2 WHERE name = $1 RETURNING *, NOW() AS observed_at",
    )
    .bind(name)
    .bind(status)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(NodeStatusChange::Changed {
        before,
        node: Box::new(node),
    })
}

pub async fn record_node_heartbeat(
    pool: &PgPool,
    public_key: &str,
    version: &str,
    addr_hint: &str,
    disk_free: Option<i64>,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE nodes SET last_seen = NOW(), version = $2, addr_hint = $3, disk_free = $4 WHERE public_key = $1",
    )
    .bind(public_key)
    .bind(version)
    .bind(addr_hint)
    .bind(disk_free)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn clear_node_heartbeat(pool: &PgPool, public_key: &str) -> Result<()> {
    sqlx::query("UPDATE nodes SET last_seen = NULL WHERE public_key = $1")
        .bind(public_key)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_node_by_id(pool: &PgPool, id: i64) -> Result<Option<ClusterNode>> {
    Ok(
        sqlx::query_as("SELECT *, NOW() AS observed_at FROM nodes WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn get_repository_by_uuid(
    pool: &PgPool,
    uuid: &str,
) -> Result<Option<crate::backend::db::models::Repository>> {
    Ok(
        sqlx::query_as("SELECT * FROM repositories WHERE right(disk_path, 40) = $1 || '.git'")
            .bind(uuid)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn get_replicas(pool: &PgPool) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT replicas FROM node_configuration WHERE id = 1")
            .fetch_optional(pool)
            .await?
            .unwrap_or_else(|| "1".to_owned()),
    )
}

pub async fn set_replicas(pool: &PgPool, value: &str) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE node_configuration SET replicas = $1, updated_at = NOW() WHERE id = 1")
            .bind(value)
            .execute(pool)
            .await?
            .rows_affected()
            == 1,
    )
}

pub async fn bump_write_version(pool: &PgPool, repo_id: i64) -> Result<()> {
    // A repository that no storage node holds has no replicas, so its changes are not counted.
    let held_by: Option<Option<i64>> =
        sqlx::query_scalar("SELECT storage_node_id FROM repositories WHERE id = $1")
            .bind(repo_id)
            .fetch_optional(pool)
            .await?;
    match held_by {
        None => bail!("repository {repo_id} not found"),
        Some(None) => Ok(()),
        Some(Some(_)) => {
            sqlx::query("UPDATE repositories SET write_version = write_version + 1 WHERE id = $1")
                .bind(repo_id)
                .execute(pool)
                .await?;
            Ok(())
        }
    }
}

pub async fn get_write_version(pool: &PgPool, repo_id: i64) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT write_version FROM repositories WHERE id = $1")
            .bind(repo_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn list_primary_repositories(
    pool: &PgPool,
    node_id: i64,
) -> Result<Vec<(i64, String, Option<i64>, Option<i64>)>> {
    Ok(sqlx::query_as(
        "SELECT r.id, r.disk_path, rr.node_id, rr.applied_version
         FROM repositories r
         LEFT JOIN repository_replicas rr ON rr.repository_id = r.id
         WHERE r.storage_node_id = $1
         ORDER BY r.id, rr.node_id",
    )
    .bind(node_id)
    .fetch_all(pool)
    .await?)
}

pub async fn list_held_replicas(pool: &PgPool, node_id: i64) -> Result<Vec<HeldReplica>> {
    Ok(sqlx::query_as(
        "SELECT rr.repository_id, r.disk_path, r.storage_node_id AS primary_node_id,
                rr.applied_version, r.write_version
         FROM repository_replicas rr
         JOIN repositories r ON r.id = rr.repository_id
         WHERE rr.node_id = $1
         ORDER BY rr.repository_id",
    )
    .bind(node_id)
    .fetch_all(pool)
    .await?)
}

pub async fn list_replicas(pool: &PgPool, repo_id: i64) -> Result<Vec<(i64, i64)>> {
    Ok(sqlx::query_as(
        "SELECT node_id, applied_version FROM repository_replicas
         WHERE repository_id = $1 ORDER BY node_id",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}

/// The fork family of the repository bound as `$1`. It walks forks in both directions: each
/// member adds its forks and its source. `UNION` stops the walk at repositories it has found.
const FORK_FAMILY: &str = "WITH RECURSIVE family(id) AS (
         SELECT CAST($1 AS BIGINT)
         UNION
         SELECT r.id FROM family f
         JOIN repositories p ON p.id = f.id
         JOIN repositories r ON r.forked_from_id = f.id OR r.id = p.forked_from_id
     )
     SELECT r.id, r.storage_node_id, r.write_version
     FROM repositories r JOIN family f ON f.id = r.id
     ORDER BY r.id";

pub async fn list_fork_family(pool: &PgPool, repo_id: i64) -> Result<Vec<(i64, Option<i64>, i64)>> {
    Ok(sqlx::query_as(FORK_FAMILY)
        .bind(repo_id)
        .fetch_all(pool)
        .await?)
}

pub async fn insert_replica(
    pool: &PgPool,
    repo_id: i64,
    node_id: i64,
    primary_id: i64,
) -> Result<bool> {
    Ok(sqlx::query(
        "INSERT INTO repository_replicas (repository_id, node_id)
         SELECT r.id, CAST($2 AS BIGINT) FROM repositories r
         WHERE r.id = $1
           AND r.storage_node_id = CAST($3 AS BIGINT)
           AND CAST($2 AS BIGINT) <> CAST($3 AS BIGINT)
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
    pool: &PgPool,
    repo_id: i64,
    node_id: i64,
    primary_id: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    // Retirement and promotion must not both act on the same copy. Recheck the primary
    // after waiting, since the plan may belong to the former primary.
    let primary = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT storage_node_id FROM repositories WHERE id = $1 FOR UPDATE",
    )
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    if primary != Some(primary_id) {
        return Ok(false);
    }
    let deleted =
        sqlx::query("DELETE FROM repository_replicas WHERE repository_id = $1 AND node_id = $2")
            .bind(repo_id)
            .bind(node_id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
    tx.commit().await?;
    Ok(deleted)
}

pub async fn set_applied_version(
    pool: &PgPool,
    repo_id: i64,
    node_id: i64,
    primary_id: i64,
    version: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    // Waits for a promotion in progress, then reads the primary it left. Without the lock
    // the check would use a snapshot from before the promotion.
    let primary = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT storage_node_id FROM repositories WHERE id = $1 FOR KEY SHARE",
    )
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    if primary != Some(primary_id) {
        return Ok(false);
    }
    let updated = sqlx::query(
        "UPDATE repository_replicas SET applied_version = $3, synced_at = NOW()
         WHERE repository_id = $1 AND node_id = $2",
    )
    .bind(repo_id)
    .bind(node_id)
    .bind(version)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(updated)
}

pub async fn replica_counts(pool: &PgPool) -> Result<Vec<NodeReplicaCounts>> {
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
async fn node_at_db_time(conn: &mut PgConnection, node: i64) -> Result<Option<ClusterNode>> {
    Ok(sqlx::query_as::<_, ClusterNode>(
        "SELECT *, clock_timestamp() AS observed_at FROM nodes WHERE id = $1",
    )
    .bind(node)
    .fetch_optional(&mut *conn)
    .await?)
}

pub async fn promote_repository(
    pool: &PgPool,
    repo_id: i64,
    from: i64,
    to: i64,
    require_current: bool,
) -> Result<PromoteOutcome> {
    // Every early return drops the transaction, which rolls it back.
    let mut tx = pool.begin().await?;
    // The row lock holds back version bumps and other promotions until this commits.
    let Some((primary, write_version)) = sqlx::query_as::<_, (Option<i64>, i64)>(
        "SELECT storage_node_id, write_version FROM repositories WHERE id = $1 FOR UPDATE",
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
        "SELECT applied_version FROM repository_replicas WHERE repository_id = $1 AND node_id = $2",
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
/// all. The family is read and its rows locked here, so a member that another node has already
/// moved is refused. Two nodes taking over one family cannot split it: the second waits for the
/// first, then finds the family moved.
pub async fn promote_fork_family(
    pool: &PgPool,
    repo_id: i64,
    from: i64,
    to: i64,
) -> Result<FamilyOutcome> {
    let mut tx = pool.begin().await?;
    // The rows are locked in id order, so two transactions over one family never wait on each
    // other in turn.
    let members: Vec<(i64, Option<i64>, i64)> =
        sqlx::query_as(&format!("{FORK_FAMILY} FOR UPDATE OF r"))
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
             WHERE repository_id = $1 AND node_id = $2",
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
    conn: &mut PgConnection,
    repo_id: i64,
    from: i64,
    to: i64,
    applied_version: i64,
) -> Result<()> {
    sqlx::query("UPDATE repositories SET storage_node_id = $2 WHERE id = $1")
        .bind(repo_id)
        .bind(to)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE repository_replicas SET applied_version = LEAST(applied_version, $3)
         WHERE repository_id = $1 AND node_id <> $2",
    )
    .bind(repo_id)
    .bind(to)
    .bind(applied_version)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM repository_replicas WHERE repository_id = $1 AND node_id = $2")
        .bind(repo_id)
        .bind(to)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO repository_replicas (repository_id, node_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(repo_id)
    .bind(from)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
