//! Drain admission reserves complete fork families before changing a node's state.
use anyhow::{ensure, Result};
use codeyard_db_core::queries::credentials::Actor;
use codeyard_db_core::{models::ClusterNode, native_operation, DbPool};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Administrator access is required.")]
    Denied,
    #[error("Node not found.")]
    Missing,
    #[error("The node is rebalancing a repository. Retry after that move finishes.")]
    Busy,
    #[error("Invalid node name.")]
    Invalid,
    #[error("The node is offline; use cluster promote --all-from instead.")]
    Offline,
    #[error("Drain needs {missing_bytes} more bytes on active storage nodes, keeping 15% free.")]
    Capacity { missing_bytes: i64 },
}

#[derive(sqlx::FromRow, Clone)]
pub(crate) struct Repository {
    pub id: i64,
    pub parent: Option<i64>,
    pub primary_id: i64,
    pub version: i64,
    pub size: i64,
}

pub(crate) fn repository_limit() -> Result<i64> {
    let limit = std::env::var("CLUSTER_DRAIN_MAX_REPOSITORIES")
        .map(|value| value.parse())
        .unwrap_or(Ok(10_000))?;
    ensure!(
        (1..=1_000_000).contains(&limit),
        "invalid drain repository limit"
    );
    Ok(limit)
}

native_operation!(crate::postgres::drain::change, crate::sqlite::drain::change,
    /// Admission, capacity reservation, state and audit share one transaction.
    fn change(pool: &DbPool, authority: Option<&Actor>, name: &str, cancel: bool, leader: Option<i64>) -> Result<ClusterNode>);
native_operation!(crate::postgres::drain::record_capacity, crate::sqlite::drain::record_capacity,
    fn record_capacity(pool: &DbPool, key: &str, total: i64) -> Result<()>);
native_operation!(crate::postgres::drain::work, crate::sqlite::drain::work,
    /// One bounded batch, ordered by repository. Targets of a family were reserved together.
    fn work(pool: &DbPool, node: i64) -> Result<Vec<(i64, i64)>>);
native_operation!(crate::postgres::drain::complete_repository, crate::sqlite::drain::complete_repository,
    fn complete_repository(pool: &DbPool, node: i64, repository: i64) -> Result<bool>);
native_operation!(crate::postgres::drain::finish, crate::sqlite::drain::finish,
    fn finish(pool: &DbPool, node: i64) -> Result<bool>);
native_operation!(crate::postgres::drain::record_error, crate::sqlite::drain::record_error,
    fn record_error(pool: &DbPool, node: i64, repository: i64) -> Result<()>);
native_operation!(crate::postgres::drain::recovering, crate::sqlite::drain::recovering,
    fn recovering(pool: &DbPool, node: i64) -> Result<Vec<i64>>);

native_operation!(crate::postgres::drain::draining_nodes, crate::sqlite::drain::draining_nodes,
    fn draining_nodes(pool: &DbPool) -> Result<Vec<i64>>);
native_operation!(crate::postgres::drain::expired_runs, crate::sqlite::drain::expired_runs,
    fn expired_runs(pool: &DbPool, maximum_seconds: i64) -> Result<Vec<(i64, String)>>);

/// `(repository, node, applied version)`. Primary data is already allocated, as are
/// current copies. A fresh copy reserves the existing staging factor in addition.
pub(crate) fn plan(
    source: i64,
    nodes: &[ClusterNode],
    repositories: &[Repository],
    copies: &[(i64, i64, i64)],
    pending: &[(i64, i64)],
    wanted: usize,
) -> Result<Vec<(i64, i64)>> {
    let mut space = BTreeMap::new();
    for node in nodes
        .iter()
        .filter(|node| node.id != source && node.state == "active" && node.is_serving_storage())
    {
        if let (Some(free), Some(_)) = (node.available_storage_bytes(), node.disk_total) {
            let reserved = pending
                .iter()
                .find(|row| row.0 == node.id)
                .map_or(0, |row| row.1);
            space.insert(node.id, free.saturating_sub(reserved).max(0));
        }
    }
    let mut links = BTreeMap::<i64, Vec<i64>>::new();
    let records: BTreeMap<_, _> = repositories.iter().map(|repo| (repo.id, repo)).collect();
    let copies: BTreeMap<_, _> = copies
        .iter()
        .map(|&(repo, node, version)| ((repo, node), version))
        .collect();
    for repo in repositories {
        if let Some(parent) = repo.parent.filter(|id| records.contains_key(id)) {
            links.entry(repo.id).or_default().push(parent);
            links.entry(parent).or_default().push(repo.id);
        }
    }
    let mut unseen: BTreeSet<_> = records.keys().copied().collect();
    let mut result = Vec::new();
    while let Some(first) = unseen.pop_first() {
        let mut family = vec![first];
        let mut index = 0;
        while index < family.len() {
            for &id in links.get(&family[index]).into_iter().flatten() {
                if unseen.remove(&id) {
                    family.push(id);
                }
            }
            index += 1;
        }
        let members: Vec<_> = family.iter().map(|id| records[id]).collect();
        let current = |repo: &Repository, node: i64| {
            repo.primary_id == node || copies.get(&(repo.id, node)) == Some(&repo.version)
        };
        let mut candidates = Vec::new();
        for (&node, &free) in &space {
            let needed = members
                .iter()
                .filter(|repo| !current(repo, node))
                .try_fold(0i64, |total, repo| {
                    repo.size
                        .max(1)
                        .checked_mul(crate::cluster::REPLICA_STAGING_FACTOR)
                        .and_then(|bytes| total.checked_add(bytes))
                        .ok_or_else(|| anyhow::anyhow!("drain reservation overflow"))
                })?;
            candidates.push((node, needed, free));
        }
        candidates.sort_by_key(|(node, needed, free)| (*needed, std::cmp::Reverse(*free), *node));
        let count = wanted;
        let selected: Vec<_> = candidates
            .iter()
            .filter(|(_, needed, free)| needed <= free)
            .take(count)
            .collect();
        if selected.len() != count {
            let missing_bytes = candidates
                .iter()
                .filter(|(_, needed, free)| needed > free)
                .map(|(_, needed, free)| needed - free)
                .min()
                .unwrap_or_else(|| members.iter().map(|repo| repo.size.max(1)).sum());
            return Err(Refusal::Capacity { missing_bytes }.into());
        }
        for &(target, needed, _) in selected {
            *space.get_mut(&target).expect("selected capacity") -= needed;
            for repo in &members {
                result.push((repo.id, target));
            }
        }
    }
    Ok(result)
}

pub(crate) const FAMILY: &str = "WITH RECURSIVE affected(id) AS (
    SELECT id FROM repositories WHERE storage_node_id=$1
    UNION SELECT repository_id FROM repository_replicas WHERE node_id=$1
    UNION SELECT repository_id FROM repository_voters WHERE node_id=$1
), family(id) AS (
    SELECT id FROM affected UNION
    SELECT r.id FROM family f JOIN repositories p ON p.id=f.id
    JOIN repositories r ON r.forked_from_id=f.id OR r.id=p.forked_from_id
)";
