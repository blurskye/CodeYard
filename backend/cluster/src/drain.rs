//! One drain operation for the panel, REST and CLI; storage work resumes on each pass.
use anyhow::{Context, Result};
use codeyard_db_core::queries::credentials::Actor;
use codeyard_db_core::{models::ClusterNode, DbPool};
use codeyard_repos_core::context::NodeContext;
use std::collections::BTreeMap;

pub async fn change(
    pool: &DbPool,
    authority: Option<&Actor>,
    name: &str,
    cancel: bool,
    leader: Option<i64>,
) -> Result<ClusterNode> {
    let name = codeyard_cluster_core::pairing::normalize_name(name)
        .map_err(|_| codeyard_db_cluster::drain::Refusal::Invalid)?;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        codeyard_db_cluster::drain::change(pool, authority, &name, cancel, leader),
    )
    .await?
}

pub async fn step(context: &NodeContext) -> Result<()> {
    let pool = context.database()?;
    for repository in codeyard_db_cluster::drain::recovering(pool, context.node_id).await? {
        // Each voter reconstructs its own retained bytes. Serving waits for every voter.
        if let Err(error) = crate::replicas::promote::recover_family(
            pool,
            repository,
            context.node_id,
            &context.repos_path,
        )
        .await
        {
            tracing::debug!(repository, %error, "canonical recovery is pending");
        }
    }
    let node = codeyard_db_cluster::cluster::get_node_by_id(pool, context.node_id)
        .await?
        .context("node membership disappeared")?;
    if node.state != "draining" || node.status != "active" {
        return Ok(());
    }
    let quorum = codeyard_db_cluster::cluster::writes::write_settings(pool)
        .await?
        .write_quorum
        .is_some();
    let mut work = BTreeMap::<i64, Vec<i64>>::new();
    for (repository, target) in codeyard_db_cluster::drain::work(pool, node.id).await? {
        work.entry(repository).or_default().push(target);
    }
    // A bounded pass advances one family, leaving the copy loop free to fill its slots.
    if let Some((&repository, targets)) = work.first_key_value() {
        let row = codeyard_db_repos::repositories::records::get_repository_by_id(pool, repository)
            .await?
            .context("drain repository disappeared")?;
        let primary = row
            .storage_node_id
            .context("drain repository has no primary")?;
        let target = if targets.contains(&primary) {
            primary
        } else {
            targets[0]
        };
        let result = if quorum {
            crate::replicas::promote::promote_to_voters(pool, repository, target, targets).await
        } else if primary == node.id {
            crate::replicas::promote::promote(pool, repository, Some(target), false).await
        } else {
            Ok(Vec::new())
        };
        match result {
            Ok(_) => {
                // Readiness and source-voter retirement are checked again in the write.
                codeyard_db_cluster::drain::complete_repository(pool, node.id, repository).await?;
            }
            Err(error) => {
                codeyard_db_cluster::drain::record_error(pool, node.id, repository).await?;
                tracing::debug!(repository, %error, "node drain is waiting");
            }
        }
    }
    codeyard_db_cluster::drain::finish(pool, node.id).await?;
    Ok(())
}
