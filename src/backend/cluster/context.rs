use super::{identity::NodeKey, rpc::unavailable, CorePeer, IrohDialer};
use crate::backend::{db::DbPool, repositories::store::RepoError};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

/// How long a node that could not be dialed is left alone. Requests for it fail at once,
/// and reads go to a copy, instead of each one waiting out the dial.
const UNREACHABLE_FOR: Duration = Duration::from_secs(5);

/// Resources belonging to one node, never process-global.
#[derive(Clone)]
pub struct NodeContext {
    pub pool: Option<DbPool>,
    /// Dials iroh peers. Its endpoint opens on first use.
    pub iroh: IrohDialer,
    /// This node's key: its identity in `nodes`, and its TLS identity over TCP.
    pub key: NodeKey,
    pub node_id: i64,
    pub repos_path: PathBuf,
    /// Core nodes a database-free worker dials, by node id.
    pub peers: Arc<RwLock<HashMap<i64, CorePeer>>>,
    /// Nodes a dial failed for, and when.
    pub unreachable: Arc<Mutex<HashMap<i64, Instant>>>,
    /// Read targets supplied by the core for a database-free CI claim.
    pub ci_replicas: Arc<RwLock<HashMap<uuid::Uuid, Vec<i64>>>>,
}

impl std::fmt::Debug for NodeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeContext")
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl NodeContext {
    pub fn database(&self) -> Result<&DbPool, RepoError> {
        self.pool.as_ref().ok_or_else(unavailable)
    }

    pub(super) fn recently_unreachable(&self, node_id: i64) -> bool {
        self.unreachable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&node_id)
            .is_some_and(|failed| failed.elapsed() < UNREACHABLE_FOR)
    }

    pub(super) fn note_dial(&self, node_id: i64, reached: bool) {
        let mut unreachable = self.unreachable.lock().unwrap_or_else(|e| e.into_inner());
        if reached {
            unreachable.remove(&node_id);
        } else {
            unreachable.insert(node_id, Instant::now());
        }
    }

    /// A worker learns storage addresses with its claim, without a database lookup.
    pub fn remember_peer(&self, id: i64, peer: CorePeer) {
        self.peers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, peer);
    }
}
