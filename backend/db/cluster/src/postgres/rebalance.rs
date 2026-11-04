use crate::{drain::Repository, rebalance::Move};
use anyhow::{ensure, Result};
use codeyard_db_core::models::ClusterNode;
use sqlx::PgPool;

pub async fn plan(pool: &PgPool, node: i64) -> Result<Option<Move>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(37004)")
        .execute(&mut *tx)
        .await?;
    if let Some(row) = sqlx::query_as::<_, (i64,i64,i64,String,String)>("SELECT repository_id,source_node_id,target_node_id,targets::text,terms::text FROM repository_moves WHERE source_node_id=$1 LIMIT 1 FOR UPDATE").bind(node).fetch_optional(&mut *tx).await? {
        return Ok(Some(crate::rebalance::decode(row)?));
    }
    let mut nodes: Vec<ClusterNode> = sqlx::query_as(
        "SELECT *,clock_timestamp() AS observed_at FROM nodes ORDER BY id LIMIT 1001 FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await?;
    ensure!(nodes.len() <= 1000, "rebalance node limit exceeded");
    let source = nodes.iter().find(|item| item.id == node);
    if !source.is_some_and(|item| {
        item.state == "active"
            && item.is_serving_storage()
            && item.disk_total.is_some_and(|total| {
                total > 0 && (item.disk_free.unwrap_or(total) as f64) < total as f64 * 0.2
            })
    }) {
        return Ok(None);
    }
    let busy: Vec<i64> =
        sqlx::query_scalar("SELECT node_id FROM repository_move_nodes ORDER BY node_id LIMIT 1001")
            .fetch_all(&mut *tx)
            .await?;
    ensure!(busy.len() <= 1000, "rebalance node limit exceeded");
    if busy.contains(&node) {
        return Ok(None);
    }
    nodes.retain(|item| {
        item.id == node
            || (!busy.contains(&item.id)
                && item.disk_total.is_some_and(|total| {
                    total > 0
                        && item
                            .disk_free
                            .is_some_and(|free| free as f64 > total as f64 * 0.4)
                }))
    });
    let setting: String = sqlx::query_scalar("SELECT replicas FROM node_configuration WHERE id=1")
        .fetch_one(&mut *tx)
        .await?;
    if setting == "all" {
        return Ok(None);
    }
    let quorum: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM node_configuration WHERE write_quorum IS NOT NULL)",
    )
    .fetch_one(&mut *tx)
    .await?;
    let wanted = if quorum { 2 } else { setting.parse::<usize>()? };
    let candidates: Vec<i64> = sqlx::query_scalar("SELECT r.id FROM repositories r WHERE r.storage_node_id=$1 AND NOT EXISTS(SELECT 1 FROM node_drain_repositories d WHERE d.repository_id=r.id) ORDER BY r.size_bytes,COALESCE((SELECT MAX(created_at) FROM repository_pushes WHERE repo_id=r.id),r.created_at),r.id LIMIT 128")
        .bind(node).fetch_all(&mut *tx).await?;
    let limit = crate::drain::repository_limit()?;
    let pending: Vec<(i64,i64)> = sqlx::query_as("SELECT rr.node_id,CAST(SUM(GREATEST(r.size_bytes,1))*2 AS BIGINT) FROM repository_replicas rr JOIN repositories r ON r.id=rr.repository_id WHERE rr.applied_version<0 AND rr.synced_at IS NULL GROUP BY rr.node_id LIMIT 1001")
        .fetch_all(&mut *tx).await?;
    ensure!(pending.len() <= 1000, "rebalance node limit exceeded");
    for repository in candidates {
        let family = crate::postgres::cluster::FORK_FAMILY;
        let rows: Vec<Repository> = sqlx::query_as(&format!("WITH family AS ({family}) SELECT r.id,r.forked_from_id AS parent,r.storage_node_id AS primary_id,r.write_version AS version,r.size_bytes AS size FROM repositories r JOIN family f ON f.id=r.id ORDER BY r.id LIMIT $2 FOR UPDATE OF r"))
            .bind(repository).bind(limit+1).fetch_all(&mut *tx).await?;
        if rows.len() as i64 > limit || rows.iter().any(|row| row.primary_id != node) {
            continue;
        }
        let copies: Vec<(i64,i64,i64)> = sqlx::query_as(&format!("WITH family AS ({family}) SELECT repository_id,node_id,MAX(applied_version) FROM (SELECT rr.repository_id,rr.node_id,rr.applied_version FROM repository_replicas rr JOIN family f ON f.id=rr.repository_id UNION ALL SELECT v.repository_id,v.node_id,v.applied_version FROM repository_voters v JOIN family f ON f.id=v.repository_id) copies GROUP BY repository_id,node_id ORDER BY repository_id,node_id LIMIT $2"))
            .bind(repository).bind(limit.saturating_mul(1000)+1).fetch_all(&mut *tx).await?;
        ensure!(
            copies.len() as i64 <= limit.saturating_mul(1000),
            "rebalance copy limit exceeded"
        );
        let planned = match crate::drain::plan(node, &nodes, &rows, &copies, &pending, wanted) {
            Ok(plan) => plan,
            Err(error) if error.is::<crate::drain::Refusal>() => continue,
            Err(error) => return Err(error),
        };
        let targets: Vec<_> = planned
            .iter()
            .map(|row| row.1)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let Some(target) = targets
            .iter()
            .max_by_key(|target| {
                nodes
                    .iter()
                    .find(|node| node.id == **target)
                    .and_then(|node| node.disk_free)
                    .unwrap_or(0)
            })
            .copied()
        else {
            continue;
        };
        let terms: Vec<(i64,i64)> = sqlx::query_as(&format!("WITH family AS ({family}) SELECT r.id,COALESCE(c.term,0) FROM repositories r JOIN family f ON f.id=r.id LEFT JOIN repository_write_control c ON c.repository_id=r.id ORDER BY r.id LIMIT $2"))
            .bind(repository).bind(limit+1).fetch_all(&mut *tx).await?;
        sqlx::query("INSERT INTO repository_moves(repository_id,source_node_id,target_node_id,targets,terms) VALUES($1,$2,$3,$4,$5)")
            .bind(repository).bind(node).bind(target).bind(serde_json::to_value(&targets)?).bind(serde_json::to_value(&terms)?).execute(&mut *tx).await?;
        for participant in std::iter::once(node).chain(targets.iter().copied()) {
            sqlx::query("INSERT INTO repository_move_nodes(node_id,repository_id) VALUES($1,$2)")
                .bind(participant)
                .bind(repository)
                .execute(&mut *tx)
                .await?;
        }
        for (member, destination) in planned {
            sqlx::query("INSERT INTO repository_replicas(repository_id,node_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(member).bind(destination).execute(&mut *tx).await?;
        }
        let metadata = serde_json::json!({"repository":repository,"from":node,"to":target});
        sqlx::query("INSERT INTO audit_log(action,target_type,target_id,metadata) VALUES('repository.rebalance','repository',$1,$2)").bind(repository).bind(metadata).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(Some(Move {
            repository,
            source: node,
            target,
            targets,
            terms,
        }));
    }
    tx.commit().await?;
    Ok(None)
}

pub async fn retire_source(pool: &PgPool, movement: &Move) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND status='active' AND state='active')",
    )
    .bind(movement.source)
    .fetch_one(&mut *tx)
    .await?;
    if !active {
        return Ok(false);
    }
    for (repository, _) in &movement.terms {
        let row: Option<(Option<i64>, i64)> = sqlx::query_as(
            "SELECT storage_node_id,write_version FROM repositories WHERE id=$1 FOR UPDATE",
        )
        .bind(repository)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((Some(primary), version)) = row else {
            return Ok(false);
        };
        if primary != movement.target {
            return Ok(false);
        }
        let still_voting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repository_voters WHERE repository_id=$1 AND node_id=$2) OR EXISTS(SELECT 1 FROM repository_write_control WHERE repository_id=$1 AND serving=0)")
            .bind(repository).bind(movement.source).fetch_one(&mut *tx).await?;
        if still_voting {
            return Ok(false);
        }
        for target in &movement.targets {
            let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes n WHERE n.id=$2 AND n.status='active' AND n.state='active' AND ($2=$4 OR EXISTS(SELECT 1 FROM repository_replicas WHERE repository_id=$1 AND node_id=$2 AND applied_version=$3) OR EXISTS(SELECT 1 FROM repository_voters WHERE repository_id=$1 AND node_id=$2 AND applied_version=$3)))")
                .bind(repository).bind(target).bind(version).bind(primary).fetch_one(&mut *tx).await?;
            if !current {
                return Ok(false);
            }
        }
    }
    for (repository, _) in &movement.terms {
        sqlx::query("DELETE FROM repository_replicas WHERE repository_id=$1 AND node_id=$2")
            .bind(repository)
            .bind(movement.source)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn finish(pool: &PgPool, movement: &Move) -> Result<bool> {
    Ok(sqlx::query("DELETE FROM repository_moves WHERE repository_id=$1 AND source_node_id=$2 AND target_node_id=$3")
        .bind(movement.repository).bind(movement.source).bind(movement.target).execute(pool).await?.rows_affected()==1)
}
