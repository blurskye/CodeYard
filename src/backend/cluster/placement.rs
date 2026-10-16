//! New repositories use the online storage node with the most free space. Without
//! cluster settings there is one machine and no node is recorded.
use super::NodeContext;
use crate::backend::{
    db::{
        self,
        models::{ClusterNode, Repository},
    },
    repositories::store::RepoError,
};
use std::cmp::Reverse;

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
    let mut candidates: Vec<&ClusterNode> = nodes
        .iter()
        .filter(|node| {
            node.is_online()
                && node.roles.iter().any(|role| role == "storage")
                && node.disk_free.is_some()
        })
        .collect();
    candidates.sort_by_key(|node| (Reverse(node.disk_free.unwrap_or_default()), node.id));
    candidates
}
