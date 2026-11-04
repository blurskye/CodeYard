use crate::drain::{Refusal, Repository, FAMILY};
use anyhow::{ensure, Result};
use codeyard_db_core::models::ClusterNode;
use codeyard_db_core::queries::credentials::Actor;
use sqlx::PgPool;

pub async fn change(
    pool: &PgPool,
    authority: Option<&Actor>,
    name: &str,
    cancel: bool,
) -> Result<ClusterNode> {
    let limit = crate::drain::repository_limit()?;
    let mut tx = pool.begin().await?;
    if let Some(actor) = authority {
        let credential = actor.credential().ok_or(Refusal::Denied)?;
        ensure!(
            codeyard_db_identity::postgres::users::transitions::active_admin(
                &mut tx,
                actor.user_id
            )
            .await?
                && codeyard_db_core::queries::postgres::users::transitions::credential_live(
                    &mut tx,
                    actor.user_id,
                    &credential,
                    true
                )
                .await?,
            Refusal::Denied
        );
    }
    sqlx::query("SELECT pg_advisory_xact_lock(37004)")
        .execute(&mut *tx)
        .await?;
    let node: ClusterNode = sqlx::query_as(
        "SELECT *,clock_timestamp() AS observed_at FROM nodes WHERE name=$1 FOR UPDATE",
    )
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Refusal::Missing)?;
    if cancel {
        sqlx::query("DELETE FROM node_drain_repositories WHERE node_id=$1")
            .bind(node.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE nodes SET state='active',drain_started_at=NULL,drain_error=NULL WHERE id=$1",
        )
        .bind(node.id)
        .execute(&mut *tx)
        .await?;
    } else if node.state == "active" {
        ensure!(node.is_online(), Refusal::Offline);
        let nodes: Vec<ClusterNode> = sqlx::query_as("SELECT *,clock_timestamp() AS observed_at FROM nodes ORDER BY id LIMIT 1001 FOR UPDATE")
            .fetch_all(&mut *tx).await?;
        ensure!(nodes.len() <= 1000, "drain node limit exceeded");
        let repos: Vec<Repository> = sqlx::query_as(&format!("{FAMILY} SELECT r.id,r.forked_from_id AS parent,r.storage_node_id AS primary_id,r.write_version AS version,r.size_bytes AS size FROM repositories r JOIN family f ON f.id=r.id ORDER BY r.id LIMIT $2 FOR UPDATE OF r"))
            .bind(node.id).bind(limit+1).fetch_all(&mut *tx).await?;
        ensure!(
            repos.len() as i64 <= limit,
            "drain repository limit exceeded"
        );
        let copies: Vec<(i64,i64,i64)> = sqlx::query_as(&format!("{FAMILY} SELECT repository_id,node_id,MAX(applied_version) FROM (SELECT rr.repository_id,rr.node_id,rr.applied_version FROM repository_replicas rr JOIN family f ON f.id=rr.repository_id UNION ALL SELECT v.repository_id,v.node_id,v.applied_version FROM repository_voters v JOIN family f ON f.id=v.repository_id) copies GROUP BY repository_id,node_id ORDER BY repository_id,node_id LIMIT $2"))
            .bind(node.id).bind(limit.saturating_mul(1000)+1).fetch_all(&mut *tx).await?;
        ensure!(
            copies.len() as i64 <= limit.saturating_mul(1000),
            "drain copy limit exceeded"
        );
        let pending: Vec<(i64,i64)> = sqlx::query_as("SELECT rr.node_id,CAST(SUM(GREATEST(r.size_bytes,1))*2 AS BIGINT) FROM repository_replicas rr JOIN repositories r ON r.id=rr.repository_id WHERE rr.applied_version<0 AND rr.synced_at IS NULL GROUP BY rr.node_id LIMIT 1001")
            .fetch_all(&mut *tx).await?;
        ensure!(pending.len() <= 1000, "drain node limit exceeded");
        let quorum: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM node_configuration WHERE write_quorum IS NOT NULL)",
        )
        .fetch_one(&mut *tx)
        .await?;
        let targets = crate::drain::plan(node.id, &nodes, &repos, &copies, &pending, quorum)?;
        for (repo, target) in targets {
            sqlx::query("INSERT INTO node_drain_repositories(node_id,repository_id,target_node_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
                .bind(node.id).bind(repo).bind(target).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO repository_replicas(repository_id,node_id) SELECT $1,$2 WHERE EXISTS(SELECT 1 FROM repositories WHERE id=$1 AND storage_node_id<>$2) ON CONFLICT DO NOTHING")
                .bind(repo).bind(target).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE nodes SET state='draining',drain_started_at=clock_timestamp(),drain_total=$2,drain_completed=0,drain_error=NULL WHERE id=$1")
            .bind(node.id).bind(repos.len() as i64).execute(&mut *tx).await?;
    }
    if let Some(actor) = authority {
        let credential = actor.credential().ok_or(Refusal::Denied)?;
        ensure!(
            codeyard_db_core::queries::postgres::users::transitions::credential_live(
                &mut tx,
                actor.user_id,
                &credential,
                true
            )
            .await?,
            Refusal::Denied
        );
    }
    // The account and session locks remain held through the capacity and state writes.
    let actor = authority.map(|actor| actor.user_id);
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,$2,'node',$3,$4)")
        .bind(actor).bind(if cancel { "node.cancel_drain" } else { "node.drain" })
        .bind(node.id).bind(serde_json::json!({"name":name})).execute(&mut *tx).await?;
    let node = sqlx::query_as("SELECT *,clock_timestamp() AS observed_at FROM nodes WHERE id=$1")
        .bind(node.id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(node)
}

pub async fn record_capacity(pool: &PgPool, key: &str, total: i64) -> Result<()> {
    ensure!(total >= 0, "invalid disk capacity");
    sqlx::query("UPDATE nodes SET disk_total=$2 WHERE public_key=$1")
        .bind(key)
        .bind(total)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn work(pool: &PgPool, node: i64) -> Result<Vec<(i64, i64)>> {
    Ok(sqlx::query_as("SELECT w.repository_id,w.target_node_id FROM node_drain_repositories w JOIN nodes n ON n.id=w.node_id WHERE w.node_id=$1 AND n.state='draining' AND n.status='active' ORDER BY w.repository_id,w.target_node_id LIMIT 128")
        .bind(node).fetch_all(pool).await?)
}

pub async fn complete_repository(pool: &PgPool, node: i64, repo: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM nodes WHERE id=$1 AND status='active' FOR UPDATE")
            .bind(node)
            .fetch_optional(&mut *tx)
            .await?;
    if state.as_deref() != Some("draining") {
        return Ok(false);
    }
    let ready: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM node_drain_repositories WHERE node_id=$1 AND repository_id=$2) AND NOT EXISTS(SELECT 1 FROM node_drain_repositories w JOIN repositories r ON r.id=w.repository_id JOIN nodes n ON n.id=w.target_node_id WHERE w.node_id=$1 AND w.repository_id=$2 AND (n.status<>'active' OR n.state<>'active' OR r.storage_node_id=$1 OR (r.storage_node_id<>w.target_node_id AND NOT EXISTS(SELECT 1 FROM repository_replicas rr WHERE rr.repository_id=r.id AND rr.node_id=w.target_node_id AND rr.applied_version=r.write_version) AND NOT EXISTS(SELECT 1 FROM repository_voters v JOIN repository_write_control c ON c.repository_id=v.repository_id WHERE v.repository_id=r.id AND v.node_id=w.target_node_id AND c.serving=1 AND v.term=c.term AND v.applied_version=c.committed_version)) OR EXISTS(SELECT 1 FROM repository_voters v WHERE v.repository_id=r.id AND v.node_id=$1)))")
        .bind(node).bind(repo).fetch_one(&mut *tx).await?;
    if !ready {
        return Ok(false);
    }
    sqlx::query("DELETE FROM repository_replicas WHERE repository_id=$1 AND node_id=$2")
        .bind(repo)
        .bind(node)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM node_drain_repositories WHERE node_id=$1 AND repository_id=$2")
        .bind(node)
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE nodes SET drain_completed=drain_completed+1,drain_error=NULL WHERE id=$1")
        .bind(node)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn finish(pool: &PgPool, node: i64) -> Result<bool> {
    // Claims require state=active in their own transaction. A draining runner keeps
    // heartbeating existing work; its existing maximum job deadline cancels overruns.
    Ok(sqlx::query("UPDATE nodes SET state='drained',drain_completed=drain_total,drain_error=NULL WHERE id=$1 AND state='draining' AND NOT EXISTS(SELECT 1 FROM node_drain_repositories WHERE node_id=$1) AND NOT EXISTS(SELECT 1 FROM repositories WHERE storage_node_id=$1) AND NOT EXISTS(SELECT 1 FROM repository_voters WHERE node_id=$1) AND NOT EXISTS(SELECT 1 FROM repository_replicas WHERE node_id=$1) AND NOT EXISTS(SELECT 1 FROM ci_runs c WHERE c.status='running' AND c.worker LIKE nodes.public_key||'/%')")
        .bind(node).execute(pool).await?.rows_affected() == 1)
}

pub async fn record_error(pool: &PgPool, node: i64, repo: i64) -> Result<()> {
    sqlx::query("UPDATE nodes SET drain_error=$2 WHERE id=$1 AND state='draining'")
        .bind(node)
        .bind(format!(
            "Repository {repo} is waiting for a current replacement copy."
        ))
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn recovering(pool: &PgPool, node: i64) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar("SELECT v.repository_id FROM repository_voters v JOIN repository_write_control c ON c.repository_id=v.repository_id WHERE v.node_id=$1 AND c.serving=0 ORDER BY v.repository_id LIMIT 128")
        .bind(node).fetch_all(pool).await?)
}

pub async fn draining_nodes(pool: &PgPool) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM nodes WHERE state='draining' AND status='active' ORDER BY id LIMIT 128",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn expired_runs(pool: &PgPool, maximum_seconds: i64) -> Result<Vec<(i64, String)>> {
    ensure!(maximum_seconds > 0, "invalid maximum job time");
    Ok(sqlx::query_as("SELECT c.id,c.worker FROM ci_runs c JOIN nodes n ON c.worker LIKE n.public_key||'/%' WHERE n.state='draining' AND n.status='active' AND c.status='running' AND n.drain_started_at + $1::bigint*INTERVAL '1 second' <= clock_timestamp() ORDER BY c.id LIMIT 128")
        .bind(maximum_seconds).fetch_all(pool).await?)
}
