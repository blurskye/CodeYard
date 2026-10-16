//! Which copies a repository should have. The primary is not a copy: with `Copies(n)`
//! the repository has n-1 copies, fewer when fewer storage nodes exist, and with `All`
//! it has one on every other storage node.
use crate::backend::{
    cluster::placement,
    db::models::{ClusterNode, ReplicaSetting},
};

/// A node that holds a copy of the repository. `applied` is false until the copy has
/// applied a version, so a new copy holds no data yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holder {
    pub node: i64,
    pub applied: bool,
}

/// The copies to add and to remove for one repository.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub add: Vec<i64>,
    pub remove: Vec<i64>,
}

/// `storage` holds the active storage nodes and `copies` the copies that exist now.
/// New copies go to online nodes with the most free space. Copies beyond the wanted
/// number go, offline ones first, but a copy on an offline node waits until the copies
/// that stay are online and have applied a version. A copy that is not on another active
/// storage node (the primary itself, a revoked node) always goes.
pub fn plan(
    setting: ReplicaSetting,
    primary: i64,
    storage: &[ClusterNode],
    copies: &[Holder],
) -> Plan {
    let others = storage.iter().filter(|node| node.id != primary).count();
    let wanted = match setting {
        ReplicaSetting::Copies(count) => usize::from(count).saturating_sub(1).min(others),
        ReplicaSetting::All => others,
    };
    let online = |id: i64| storage.iter().any(|node| node.id == id && node.is_online());
    let applied = |id: i64| copies.iter().any(|copy| copy.node == id && copy.applied);
    let held: Vec<i64> = copies.iter().map(|copy| copy.node).collect();
    let mut kept: Vec<i64> = held
        .iter()
        .copied()
        .filter(|&id| id != primary && storage.iter().any(|node| node.id == id))
        .collect();
    let mut missing = wanted.saturating_sub(kept.iter().filter(|&&id| online(id)).count());
    let mut add = Vec::new();
    for node in placement::ranked(storage) {
        if missing == 0 {
            break;
        }
        if node.id != primary && !held.contains(&node.id) {
            add.push(node.id);
            missing -= 1;
        }
    }
    kept.extend(add.iter().copied());
    let mut remove: Vec<i64> = held
        .iter()
        .copied()
        .filter(|id| !kept.contains(id))
        .collect();
    let surplus = kept.len().saturating_sub(wanted);
    if surplus > 0 {
        // `false` sorts before `true`, so offline copies are the first to go. Among online
        // copies, the ones without data go first, so a copy with data is the one kept.
        kept.sort_by_key(|&id| (online(id), applied(id)));
        let (retire, stay) = kept.split_at(surplus);
        // An offline copy waits until the copies that stay are online and hold data, so its
        // replacement holds data before the copy goes. Online copies do not wait.
        let stay_ready = stay.iter().all(|&id| online(id) && applied(id));
        if stay_ready || retire.iter().all(|&id| online(id)) {
            remove.extend_from_slice(retire);
        }
    }
    Plan { add, remove }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    /// An active storage node with `disk_free` bytes free, online or not.
    fn node(id: i64, disk_free: i64, online: bool) -> ClusterNode {
        ClusterNode {
            id,
            public_key: format!("{id:064x}"),
            name: format!("store-{id}"),
            roles: vec!["storage".into()],
            status: "active".into(),
            addr_hint: None,
            version: None,
            disk_free: Some(disk_free),
            last_seen: online.then(|| Utc::now() - Duration::seconds(5)),
            observed_at: Utc::now(),
            created_at: Utc::now(),
        }
    }

    /// A copy on `node`, and whether it has applied a version.
    fn holder(node: i64, applied: bool) -> Holder {
        Holder { node, applied }
    }

    #[test]
    fn one_copy_keeps_no_replicas_and_removes_any_left_over() {
        let storage = [node(1, 10, true), node(2, 20, true), node(3, 30, true)];
        assert_eq!(
            plan(
                ReplicaSetting::Copies(1),
                1,
                &storage,
                &[holder(2, true), holder(3, true)]
            ),
            Plan {
                add: vec![],
                remove: vec![2, 3]
            }
        );
        // The primary never holds a copy of its own repository.
        assert_eq!(
            plan(ReplicaSetting::Copies(1), 1, &storage, &[holder(1, true)]).remove,
            vec![1]
        );
    }

    #[test]
    fn a_count_adds_the_online_node_with_the_most_free_space() {
        let storage = [node(1, 10, true), node(2, 20, true), node(3, 30, true)];
        assert_eq!(
            plan(ReplicaSetting::Copies(2), 1, &storage, &[]),
            Plan {
                add: vec![3],
                remove: vec![]
            }
        );
        assert_eq!(
            plan(ReplicaSetting::Copies(3), 1, &storage, &[holder(3, true)]),
            Plan {
                add: vec![2],
                remove: vec![]
            }
        );
    }

    #[test]
    fn an_offline_copy_is_replaced_by_an_online_node() {
        let storage = [node(1, 10, true), node(2, 90, false), node(3, 30, true)];
        // The replacement is added, and the offline copy waits until it has data.
        assert_eq!(
            plan(ReplicaSetting::Copies(2), 1, &storage, &[holder(2, true)]),
            Plan {
                add: vec![3],
                remove: vec![]
            }
        );
        // With nothing online to take it, the offline copy stays until one is.
        let none_online = [node(1, 10, true), node(2, 90, false)];
        assert_eq!(
            plan(
                ReplicaSetting::Copies(2),
                1,
                &none_online,
                &[holder(2, true)]
            ),
            Plan::default()
        );
    }

    #[test]
    fn a_copy_on_an_offline_node_stays_until_its_replacement_has_applied() {
        let storage = [node(1, 10, true), node(2, 90, false), node(3, 30, true)];
        // Node 3 is new: it is added, and nothing is removed.
        assert_eq!(
            plan(ReplicaSetting::Copies(2), 1, &storage, &[holder(2, true)]),
            Plan {
                add: vec![3],
                remove: vec![]
            }
        );
        // Node 3 has its row but has not applied a version, so node 2 still stays.
        assert_eq!(
            plan(
                ReplicaSetting::Copies(2),
                1,
                &storage,
                &[holder(2, true), holder(3, false)]
            ),
            Plan::default()
        );
    }

    #[test]
    fn an_offline_copy_goes_once_its_replacement_has_applied() {
        let storage = [node(1, 10, true), node(2, 90, false), node(3, 30, true)];
        assert_eq!(
            plan(
                ReplicaSetting::Copies(2),
                1,
                &storage,
                &[holder(2, true), holder(3, true)]
            ),
            Plan {
                add: vec![],
                remove: vec![2]
            }
        );
    }

    #[test]
    fn an_online_copy_that_has_applied_needs_no_replacement() {
        let storage = [node(1, 10, true), node(2, 20, true), node(3, 30, true)];
        assert_eq!(
            plan(ReplicaSetting::Copies(2), 1, &storage, &[holder(2, true)]),
            Plan::default()
        );
    }

    #[test]
    fn an_online_copy_without_data_goes_before_one_with_data() {
        let storage = [node(1, 10, true), node(2, 20, true), node(3, 30, true)];
        // Node 2 was offline and is back with its data, while node 3 has not applied yet.
        assert_eq!(
            plan(
                ReplicaSetting::Copies(2),
                1,
                &storage,
                &[holder(2, true), holder(3, false)]
            ),
            Plan {
                add: vec![],
                remove: vec![3]
            }
        );
    }

    #[test]
    fn all_puts_a_copy_on_every_online_storage_node() {
        let storage = [node(1, 10, true), node(2, 20, true), node(3, 30, false)];
        assert_eq!(
            plan(ReplicaSetting::All, 1, &storage, &[]),
            Plan {
                add: vec![2],
                remove: vec![]
            }
        );
        let storage = [node(1, 10, true), node(2, 20, true), node(3, 30, true)];
        assert_eq!(
            plan(ReplicaSetting::All, 1, &storage, &[holder(2, true)]),
            Plan {
                add: vec![3],
                remove: vec![]
            }
        );
    }

    #[test]
    fn lowering_the_setting_removes_offline_copies_first() {
        let storage = [node(1, 10, true), node(2, 20, true), node(3, 30, false)];
        assert_eq!(
            plan(
                ReplicaSetting::Copies(2),
                1,
                &storage,
                &[holder(2, true), holder(3, true)]
            ),
            Plan {
                add: vec![],
                remove: vec![3]
            }
        );
    }

    #[test]
    fn a_node_that_is_no_longer_a_storage_node_loses_its_copy() {
        let storage = [node(1, 10, true), node(2, 20, true)];
        assert_eq!(
            plan(
                ReplicaSetting::Copies(2),
                1,
                &storage,
                &[holder(2, true), holder(9, true)]
            ),
            Plan {
                add: vec![],
                remove: vec![9]
            }
        );
    }
}
