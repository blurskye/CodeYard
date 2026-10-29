//! In cluster mode new repositories use the online storage node with the most free space.
//! A single-node installation, with or without runners on other machines, records no node.
use super::NodeContext;
use crate::backend::{
    db::{
        self,
        models::{ClusterNode, Repository},
    },
    repositories::store::RepoError,
};

pub async fn storage_node(
    context: Option<&NodeContext>,
    source: Option<&Repository>,
) -> Result<Option<i64>, RepoError> {
    if let Some(source) = source {
        return Ok(source.storage_node_id);
    }
    let Some(context) = context else {
        return Ok(None);
    };
    // A single-node installation keeps every repository in one place and records no owner;
    // its node connection is there only for runners.
    if !db::cluster::cluster_enabled(context.database()?)
        .await
        .map_err(|_| super::rpc::unavailable())?
    {
        return Ok(None);
    }
    let nodes = db::cluster::list_nodes(context.database()?)
        .await
        .map_err(|_| super::rpc::unavailable())?;
    // In a cluster the owner is always recorded, even when it is this node: other nodes
    // route by it. A node with no storage node to use refuses instead of keeping a
    // repository that no other node could reach.
    ranked(&nodes)
        .first()
        .map(|node| Some(node.id))
        .ok_or_else(super::rpc::unavailable)
}

/// Online storage nodes that report their free space, most free space first. Ties go
/// to the older node. New repositories and replica copies both pick from this order.
pub fn ranked(nodes: &[ClusterNode]) -> Vec<&ClusterNode> {
    ClusterNode::ranked_storage(nodes)
}
