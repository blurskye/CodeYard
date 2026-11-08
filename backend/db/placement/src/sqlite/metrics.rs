use crate::metrics::CiQueue;
use crate::metrics::NodeCount;
use crate::metrics::StorageNode;
use crate::metrics::MAX_STORAGE_NODES;
use anyhow::ensure;
use anyhow::Result;
use codeyard_database::pool::SqlitePool;

pub async fn storage_nodes(pool: &SqlitePool) -> Result<Vec<StorageNode>> {
    let nodes: Vec<StorageNode> = sqlx::query_as(
        "SELECT n.id AS node_id, n.disk_free,
            COALESCE((SELECT SUM(r.size_bytes) FROM repositories r WHERE r.storage_node_id=n.id),0)
            + COALESCE((SELECT SUM(r.size_bytes) FROM repository_replicas c
                JOIN repositories r ON r.id=c.repository_id
                WHERE c.node_id=n.id AND c.applied_version>=0
                  AND r.storage_node_id IS NOT n.id),0) AS repository_bytes
         FROM nodes n WHERE EXISTS (SELECT 1 FROM json_each(n.roles) WHERE value='storage')
         ORDER BY n.id LIMIT $1",
    )
    .bind((MAX_STORAGE_NODES + 1) as i64)
    .fetch_all(pool)
    .await?;
    ensure!(
        nodes.len() <= MAX_STORAGE_NODES,
        "too many storage nodes for metrics"
    );
    Ok(nodes)
}

pub async fn replica_lag(pool: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(MAX(MAX(r.write_version-c.applied_version,0)),0)
         FROM repository_replicas c JOIN repositories r ON r.id=c.repository_id",
    )
    .fetch_one(pool)
    .await?)
}

pub async fn ci_queue(pool: &SqlitePool) -> Result<CiQueue> {
    Ok(sqlx::query_as(
        "SELECT COUNT(*) AS queued,
            MAX(COALESCE(CAST(strftime('%s','now') AS INTEGER)-CAST(strftime('%s',MIN(created_at)) AS INTEGER),0),0) AS oldest_seconds
         FROM ci_runs WHERE status='queued'",
    )
    .fetch_one(pool)
    .await?)
}

pub async fn node_counts(pool: &SqlitePool) -> Result<Vec<NodeCount>> {
    Ok(
        sqlx::query_as("SELECT status, COUNT(*) AS count FROM nodes GROUP BY status LIMIT 3")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn drains(pool: &SqlitePool) -> Result<Vec<crate::metrics::Drain>> {
    let rows: Vec<crate::metrics::Drain> = sqlx::query_as("SELECT id AS node_id,state,drain_total AS total,drain_completed AS completed FROM nodes WHERE state<>'active' ORDER BY id LIMIT $1")
        .bind((MAX_STORAGE_NODES+1) as i64).fetch_all(pool).await?;
    ensure!(
        rows.len() <= MAX_STORAGE_NODES,
        "too many draining nodes for metrics"
    );
    Ok(rows)
}
