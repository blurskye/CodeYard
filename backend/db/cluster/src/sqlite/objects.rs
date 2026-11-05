use crate::objects::Object;
use anyhow::{ensure, Result};
use codeyard_db_core::models::ClusterNode;
use codeyard_db_core::pool::SqlitePool;
use sqlx::SqliteConnection;

async fn owner(conn: &mut SqliteConnection, id: i64, key: &str) -> Result<()> {
    let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND public_key=$2 AND status='active' AND state='active' AND EXISTS(SELECT 1 FROM json_each(nodes.roles) WHERE value IN ('web','ssh')))")
        .bind(id).bind(key).fetch_one(conn).await?;
    ensure!(live, "object storage caller unavailable");
    Ok(())
}

pub async fn get(pool: &SqlitePool, key: &str) -> Result<Option<Object>> {
    Ok(sqlx::query_as("SELECT object_key,digest,bytes,content_type,confirmed FROM storage_objects WHERE object_key=$1")
        .bind(key).fetch_optional(pool).await?)
}
pub async fn copies(pool: &SqlitePool, key: &str) -> Result<Vec<(i64, i32)>> {
    let rows = sqlx::query_as("SELECT node_id,ready FROM storage_object_copies WHERE object_key=$1 ORDER BY node_id LIMIT 1001")
        .bind(key).fetch_all(pool).await?;
    ensure!(rows.len() <= 1000, "object copy limit exceeded");
    Ok(rows)
}

pub async fn reserve(pool: &SqlitePool, id: i64, key: &str, object: &Object) -> Result<Vec<i64>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    owner(&mut tx, id, key).await?;
    let nodes: Vec<ClusterNode> = sqlx::query_as("SELECT *,strftime('%Y-%m-%dT%H:%M:%f000Z','now') AS observed_at FROM nodes ORDER BY id LIMIT 1001 ")
        .fetch_all(&mut *tx).await?;
    ensure!(nodes.len() <= 1000, "object node limit exceeded");
    sqlx::query("INSERT INTO storage_objects(object_key,digest,bytes,content_type) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
        .bind(&object.object_key).bind(&object.digest).bind(object.bytes).bind(&object.content_type).execute(&mut *tx).await?;
    let stored: Object = sqlx::query_as("SELECT object_key,digest,bytes,content_type,confirmed FROM storage_objects WHERE object_key=$1 ")
        .bind(&object.object_key).fetch_one(&mut *tx).await?;
    ensure!(
        stored.confirmed >= 0
            && stored.digest == object.digest
            && stored.bytes == object.bytes
            && stored.content_type == object.content_type,
        "object declaration changed"
    );
    let copies: Vec<(i64,i32)> = sqlx::query_as("SELECT node_id,ready FROM storage_object_copies WHERE object_key=$1 ORDER BY node_id LIMIT 1001")
        .bind(&object.object_key).fetch_all(&mut *tx).await?;
    ensure!(copies.len() <= 1000, "object copy limit exceeded");
    let pending = pending(&mut tx).await?;
    let targets = crate::objects::targets(&nodes, &copies, &pending, object.bytes)?;
    for node in &targets {
        sqlx::query("INSERT INTO storage_object_copies(object_key,node_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(&object.object_key).bind(node).execute(&mut *tx).await?;
    }
    owner(&mut tx, id, key).await?;
    tx.commit().await?;
    Ok(targets)
}

async fn admission(
    conn: &mut SqliteConnection,
    key: &str,
    node: i64,
    server_key: &str,
    caller: &str,
    write: bool,
    delete: bool,
) -> Result<Object> {
    // A runner never receives arbitrary object bytes; all its file access goes through
    // the web API's job-scoped credential checks. Storage peers must hold a copy assignment.
    let nodes: Vec<ClusterNode> = sqlx::query_as("SELECT *,strftime('%Y-%m-%dT%H:%M:%f000Z','now') AS observed_at FROM nodes WHERE id=$1 OR public_key=$2 ORDER BY id LIMIT 2 ")
        .bind(node).bind(caller).fetch_all(&mut *conn).await?;
    let server = nodes
        .iter()
        .find(|item| item.id == node)
        .ok_or_else(|| anyhow::anyhow!("object server unavailable"))?;
    let peer = nodes
        .iter()
        .find(|item| item.public_key == caller)
        .ok_or_else(|| anyhow::anyhow!("object caller unavailable"))?;
    ensure!(
        server.public_key == server_key
            && server.status == "active"
            && server.roles.iter().any(|role| role == "storage")
            && (!write || server.state == "active"),
        "object server unavailable"
    );
    let core = peer.roles.iter().any(|role| role == "web" || role == "ssh");
    let copy: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM storage_object_copies WHERE object_key=$1 AND node_id=$2 )",
    )
    .bind(key)
    .bind(peer.id)
    .fetch_one(&mut *conn)
    .await?;
    ensure!(
        peer.status == "active"
            && (core
                || ((!delete || peer.id == server.id)
                    && peer.roles.iter().any(|role| role == "storage")
                    && copy)),
        "object caller unavailable"
    );
    let object: Object=sqlx::query_as("SELECT o.object_key,o.digest,o.bytes,o.content_type,o.confirmed FROM storage_objects o JOIN storage_object_copies c ON c.object_key=o.object_key WHERE o.object_key=$1 AND c.node_id=$2 AND (($3 AND (o.confirmed=-1 OR (c.ready=-1 AND $5))) OR (NOT $3 AND o.confirmed>=0 AND c.ready>=0 AND ($4 OR c.ready=1)))")
        .bind(key).bind(node).bind(delete).bind(write).bind(peer.id==server.id).fetch_one(&mut *conn).await?;
    Ok(object)
}

pub async fn authorize(
    pool: &SqlitePool,
    key: &str,
    node: i64,
    server_key: &str,
    caller: &str,
    write: bool,
    delete: bool,
) -> Result<Object> {
    let mut conn = pool.acquire().await?;
    admission(&mut conn, key, node, server_key, caller, write, delete).await
}

pub async fn ready(
    pool: &SqlitePool,
    key: &str,
    node: i64,
    server_key: &str,
    caller: &str,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    admission(&mut tx, key, node, server_key, caller, true, false).await?;
    sqlx::query(
        "UPDATE storage_object_copies SET ready=1 WHERE object_key=$1 AND node_id=$2 AND ready>=0",
    )
    .bind(key)
    .bind(node)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn confirm(pool: &SqlitePool, id: i64, public_key: &str, key: &str) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    owner(&mut tx, id, public_key).await?;
    let ready:i64=sqlx::query_scalar("SELECT COUNT(*) FROM storage_object_copies c JOIN nodes n ON n.id=c.node_id WHERE c.object_key=$1 AND c.ready=1 AND n.status='active' AND n.state='active'")
        .bind(key).fetch_one(&mut *tx).await?;
    ensure!(ready >= 2, "object write quorum unavailable");
    sqlx::query("UPDATE storage_objects SET confirmed=1 WHERE object_key=$1 AND confirmed>=0")
        .bind(key)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn begin_delete(pool: &SqlitePool, id: i64, public_key: &str, key: &str) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    owner(&mut tx, id, public_key).await?;
    let result = sqlx::query("UPDATE storage_objects SET confirmed=-1,deleted_at=COALESCE(deleted_at,strftime('%Y-%m-%dT%H:%M:%f000Z','now')) WHERE object_key=$1")
        .bind(key)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}
pub async fn forget_copy(
    pool: &SqlitePool,
    key: &str,
    node: i64,
    server_key: &str,
    caller: &str,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    admission(&mut tx, key, node, server_key, caller, false, true).await?;
    let retiring:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM storage_object_copies WHERE object_key=$1 AND node_id=$2 AND ready=-1)").bind(key).bind(node).fetch_one(&mut *tx).await?;
    sqlx::query("DELETE FROM storage_object_copies WHERE object_key=$1 AND node_id=$2")
        .bind(key)
        .bind(node)
        .execute(&mut *tx)
        .await?;
    if retiring {
        sqlx::query(
            "UPDATE nodes SET drain_completed=drain_completed+1 WHERE id=$1 AND state='draining'",
        )
        .bind(node)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn finish_delete(pool: &SqlitePool, key: &str) -> Result<()> {
    let complete:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM storage_objects WHERE object_key=$1 AND confirmed=-1 AND NOT EXISTS(SELECT 1 FROM storage_object_copies WHERE object_key=$1))").bind(key).fetch_one(pool).await?;
    ensure!(complete, "object deletion is incomplete");
    Ok(())
}

pub async fn purge(pool: &SqlitePool) -> Result<()> {
    // Keep immutable-key tombstones past every admitted upload and the backup window.
    sqlx::query("DELETE FROM storage_objects WHERE object_key IN (SELECT o.object_key FROM storage_objects o WHERE o.confirmed=-1 AND o.deleted_at<strftime('%Y-%m-%dT%H:%M:%f000Z','now','-'||$1||' seconds') AND NOT EXISTS(SELECT 1 FROM storage_object_copies c WHERE c.object_key=o.object_key) ORDER BY o.deleted_at LIMIT 100)")
        .bind(codeyard_db_core::queries::files::RESTORE_WINDOW_SECONDS).execute(pool).await?;
    Ok(())
}

/// Both Git copies and file copies reserve their staging bytes against the same disks.
pub(crate) async fn pending(conn: &mut SqliteConnection) -> Result<Vec<(i64, i64)>> {
    let rows:Vec<(i64,i64)>=sqlx::query_as("SELECT node_id,CAST(SUM(bytes) AS BIGINT) FROM (SELECT c.node_id,CAST(SUM(o.bytes)*2 AS BIGINT) AS bytes FROM storage_object_copies c JOIN storage_objects o ON o.object_key=c.object_key WHERE c.ready=0 GROUP BY c.node_id UNION ALL SELECT rr.node_id,CAST(SUM(MAX(r.size_bytes,1))*2 AS BIGINT) AS bytes FROM repository_replicas rr JOIN repositories r ON r.id=rr.repository_id WHERE rr.applied_version<0 AND rr.synced_at IS NULL GROUP BY rr.node_id) reservations GROUP BY node_id LIMIT 1001")
        .fetch_all(conn).await?;
    ensure!(
        rows.len() <= 1000,
        "storage reservation node limit exceeded"
    );
    Ok(rows)
}

pub(crate) async fn prepare_drain(
    conn: &mut SqliteConnection,
    source: i64,
    nodes: &[ClusterNode],
) -> Result<i64> {
    let limit = crate::drain::repository_limit()?;
    let objects:Vec<Object>=sqlx::query_as("SELECT o.object_key,o.digest,o.bytes,o.content_type,o.confirmed FROM storage_objects o JOIN storage_object_copies c ON c.object_key=o.object_key WHERE c.node_id=$1 AND o.confirmed>=0 ORDER BY o.object_key LIMIT $2 ")
        .bind(source).bind(limit+1).fetch_all(&mut *conn).await?;
    ensure!(objects.len() as i64 <= limit, "drain object limit exceeded");
    let pending = pending(conn).await?;
    let mut space = crate::drain::storage_space(source, nodes, &pending);
    for object in &objects {
        let copies:Vec<(i64,i32)>=sqlx::query_as("SELECT node_id,ready FROM storage_object_copies WHERE object_key=$1 ORDER BY node_id LIMIT 1001")
            .bind(&object.object_key).fetch_all(&mut *conn).await?;
        ensure!(copies.len() <= 1000, "object copy limit exceeded");
        let mut choices = Vec::new();
        for (&node, &free) in &space {
            let current = copies.iter().find(|copy| copy.0 == node);
            if current.is_some_and(|copy| copy.1 < 0) {
                continue;
            }
            let needed = if current.is_some() {
                0
            } else {
                object.bytes.max(1) * 2
            };
            choices.push((current.is_some_and(|copy| copy.1 == 1), node, needed, free));
        }
        choices.sort_by_key(|(ready, node, needed, free)| {
            (
                std::cmp::Reverse(*ready),
                *needed,
                std::cmp::Reverse(*free),
                *node,
            )
        });
        let selected: Vec<_> = choices
            .iter()
            .filter(|(_, _, needed, free)| needed <= free)
            .take(2)
            .collect();
        if selected.len() != 2 {
            return Err(crate::drain::Refusal::Capacity {
                missing_bytes: choices
                    .iter()
                    .filter(|(_, _, needed, free)| needed > free)
                    .map(|(_, _, needed, free)| needed - free)
                    .min()
                    .unwrap_or(object.bytes.max(1)),
            }
            .into());
        }
        for &&(_, target, needed, _) in &selected {
            *space.get_mut(&target).expect("selected object capacity") -= needed;
            sqlx::query("INSERT INTO storage_object_copies(object_key,node_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
                .bind(&object.object_key).bind(target).execute(&mut *conn).await?;
        }
    }
    Ok(objects.len() as i64)
}

pub async fn work(pool: &SqlitePool, node: i64) -> Result<Vec<(String, i32)>> {
    Ok(sqlx::query_as("SELECT c.object_key,c.ready FROM storage_object_copies c JOIN storage_objects o ON o.object_key=c.object_key JOIN nodes n ON n.id=c.node_id WHERE c.node_id=$1 AND n.status='active' AND (c.ready=-1 OR (o.confirmed>=0 AND (c.ready=0 OR n.state='draining'))) ORDER BY c.ready,c.object_key LIMIT 32")
        .bind(node).fetch_all(pool).await?)
}

pub async fn retire(pool: &SqlitePool, node: i64, public_key: &str, key: &str) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draining:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND public_key=$2 AND status='active' AND state='draining')")
        .bind(node).bind(public_key).fetch_one(&mut *tx).await?;
    if !draining {
        return Ok(false);
    }
    let _object: Option<String> =
        sqlx::query_scalar("SELECT object_key FROM storage_objects WHERE object_key=$1 ")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
    let ready:i64=sqlx::query_scalar("SELECT COUNT(*) FROM storage_object_copies c JOIN nodes n ON n.id=c.node_id WHERE c.object_key=$1 AND c.node_id<>$2 AND c.ready=1 AND n.status='active' AND n.state='active'")
        .bind(key).bind(node).fetch_one(&mut *tx).await?;
    if ready < 2 {
        return Ok(false);
    }
    let changed =
        sqlx::query("UPDATE storage_object_copies SET ready=-1 WHERE object_key=$1 AND node_id=$2")
            .bind(key)
            .bind(node)
            .execute(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(changed.rows_affected() == 1)
}
