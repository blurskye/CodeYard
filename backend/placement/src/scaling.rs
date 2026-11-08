//! One lease leader, one evaluation per database minute, and the existing add/drain paths.
use anyhow::Result;
use codeyard_git_storage::context::NodeContext;

pub async fn step(context: &NodeContext) -> Result<()> {
    let Some(action) =
        codeyard_db_placement::scaling::evaluate(context.database()?, context.node_id).await?
    else {
        return Ok(());
    };
    tracing::info!(
        role = action.role,
        count = action.count,
        node = action.drain_node,
        dry_run = action.dry_run,
        "cluster scaling decision"
    );
    if action.dry_run {
        return Ok(());
    }
    if let Some(name) = action.drain_node {
        // Only runner-only nodes are selected by the database policy evaluator.
        crate::drain::change(
            context.database()?,
            None,
            &name,
            false,
            Some(context.node_id),
        )
        .await?;
    } else {
        let added = crate::provision::add(context, None, &action.role, action.count).await?;
        tracing::info!(
            role = action.role,
            provisioning = added.provisioning,
            "cluster scaling invitation issued"
        );
    }
    Ok(())
}
