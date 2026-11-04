//! Rebalancing uses the existing replica copier, fenced family promotion and retained paths.
use anyhow::{ensure, Context, Result};
use codeyard_db_cluster::rebalance::Move;
use codeyard_repos_core::context::NodeContext;
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub async fn step(context: &NodeContext) -> Result<()> {
    let pool = context.database()?;
    let Some(movement) = codeyard_db_cluster::rebalance::plan(pool, context.node_id).await? else {
        return Ok(());
    };
    let quorum = codeyard_db_cluster::cluster::writes::write_settings(pool)
        .await?
        .write_quorum
        .is_some();
    let row =
        codeyard_db_repos::repositories::records::get_repository_by_id(pool, movement.repository)
            .await?
            .context("moving repository disappeared")?;
    if row.storage_node_id != Some(movement.target) {
        let moved = if quorum {
            super::promote::promote_to_voters(
                pool,
                movement.repository,
                movement.target,
                &movement.targets,
            )
            .await
        } else {
            super::promote::promote(pool, movement.repository, Some(movement.target), false).await
        };
        if let Err(error) = moved {
            tracing::debug!(repository=movement.repository, %error, "rebalancing waits for current copies");
            return Ok(());
        }
    }
    if !codeyard_db_cluster::rebalance::retire_source(pool, &movement).await? {
        return Ok(());
    }
    retire_paths(context, &movement).await?;
    codeyard_db_cluster::rebalance::finish(pool, &movement).await?;
    Ok(())
}

async fn retire_paths(context: &NodeContext, movement: &Move) -> Result<()> {
    for (repository, term) in &movement.terms {
        let row = codeyard_db_repos::repositories::records::get_repository_by_id(
            context.database()?,
            *repository,
        )
        .await?
        .context("moving repository disappeared")?;
        ensure!(
            row.storage_node_id == Some(movement.target),
            "move ownership changed"
        );
        let uuid = codeyard_repos_core::wire::repository_uuid(&row.disk_path)?;
        let primary =
            codeyard_repos_core::repositories::store::primary_path(&context.repos_path, uuid);
        let mirror = codeyard_repos_core::cluster::replicas::copy_path(&context.repos_path, uuid);
        for path in [primary, mirror] {
            remove_retired(&context.repos_path, &path).await?;
        }
        if *term > 0 {
            remove_retired(
                &context.repos_path,
                &codeyard_repos_core::context::term_path(&context.repos_path, uuid, *term),
            )
            .await?;
        }
    }
    Ok(())
}

async fn remove_retired(root: &Path, path: &Path) -> Result<()> {
    if !tokio::fs::try_exists(path).await? {
        return Ok(());
    }
    codeyard_repos_core::context::literal_directory(root, path).await?;
    let guards = tokio::time::timeout(Duration::from_secs(2), async {
        let writer = codeyard_git::locks::lock(path).await;
        let publication = codeyard_git::locks::publication(path).await?;
        let readers = codeyard_git::locks::exclusive(path).await;
        Ok::<_, anyhow::Error>((writer, publication, readers))
    })
    .await??;
    let path = path.to_owned();
    codeyard_git::repository::process::blocking_until(
        Instant::now() + Duration::from_secs(30),
        move || {
            let _guards = guards;
            match std::fs::remove_dir_all(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        },
    )
    .await
}
