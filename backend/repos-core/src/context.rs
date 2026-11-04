use crate::wire::unavailable;
use codeyard_cluster_core::transport::IrohDialer;
use codeyard_cluster_core::worker::CorePeer;
use codeyard_db_core::DbPool;
use codeyard_git::error::RepoError;
use codeyard_support::node_identity::NodeKey;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::Duration;
use std::time::Instant;

/// How long a node that could not be dialed is left alone. Requests for it fail at once,
/// and reads go to a copy, instead of each one waiting out the dial.
const UNREACHABLE_FOR: Duration = Duration::from_secs(5);

async fn active_storage(pool: &DbPool, node: i64, key: Option<&str>) -> Result<(), RepoError> {
    let active = codeyard_db_cluster::cluster::get_node_by_id(pool, node)
        .await
        .map_err(|_| unavailable())?
        .is_some_and(|node| {
            node.status == "active"
                && node.roles.iter().any(|role| role == "storage")
                && key.is_none_or(|key| node.public_key == key)
        });
    if !active {
        return Err(unavailable());
    }
    Ok(())
}

fn current_copy(
    view: &codeyard_db_core::models::RecoveryView,
    node: i64,
    read_only: bool,
    expected_term: Option<i64>,
) -> bool {
    view.serving
        && expected_term.is_none_or(|term| term == view.term)
        && (read_only || view.primary_id == node)
        && view.voters.iter().any(|voter| {
            voter.node_id == node
                && voter.term == view.term
                && voter.applied_version == view.committed_version
        })
}

/// Checks the literal directory handed to a receive child, including stores made by RPC.
pub(crate) async fn check_term_directory(
    pool: &DbPool,
    repository: &codeyard_db_core::models::Repository,
    node: Option<i64>,
    path: &Path,
) -> Result<(), RepoError> {
    if repository.storage_node_id.is_none() {
        return Ok(());
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        if codeyard_db_cluster::cluster::writes::write_settings(pool)
            .await
            .map_err(|_| unavailable())?
            .write_quorum
            .is_none()
        {
            return Ok(());
        }
        let view = codeyard_db_cluster::cluster::writes::recovery_view(pool, repository.id)
            .await
            .map_err(|_| unavailable())?;
        let node = node.ok_or_else(unavailable)?;
        active_storage(pool, node, None).await?;
        if !current_copy(&view, node, false, None) {
            return Err(unavailable());
        }
        let root = path.ancestors().nth(4).ok_or_else(unavailable)?;
        let uuid = crate::cluster::rpc::repository::repository_uuid(&repository.disk_path)?;
        if term_path(root, uuid, view.term) != path {
            return Err(unavailable());
        }
        literal_directory(root, path).await
    })
    .await
    .map_err(|_| unavailable())?
}

/// A tenure owns a directory for its lifetime, including any surviving Git children.
/// Promotion reconstructs a new directory; it never renames or retargets the old one.
pub fn term_path(root: &Path, uuid: uuid::Uuid, term: i64) -> PathBuf {
    root.join("terms")
        .join(uuid.to_string())
        .join(term.to_string())
        .join(format!("{uuid}.git"))
}

pub(crate) async fn literal_directory(root: &Path, path: &Path) -> Result<(), RepoError> {
    let relative = path.strip_prefix(root).map_err(|_| unavailable())?;
    let mut current = root.to_owned();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(unavailable());
        }
        current.push(component);
        let metadata = tokio::fs::symlink_metadata(&current)
            .await
            .map_err(|_| unavailable())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(unavailable());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_have_distinct_literal_directories() {
        let uuid = uuid::Uuid::new_v4();
        let root = Path::new("/repos");
        assert_eq!(
            term_path(root, uuid, 7),
            root.join("terms")
                .join(uuid.to_string())
                .join("7")
                .join(format!("{uuid}.git"))
        );
        assert_ne!(term_path(root, uuid, 7), term_path(root, uuid, 8));
    }

    #[tokio::test]
    async fn term_admission_rejects_missing_directories_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let path = term_path(root.path(), uuid::Uuid::new_v4(), 1);
        assert!(literal_directory(root.path(), &path).await.is_err());
        tokio::fs::create_dir_all(&path).await.unwrap();
        literal_directory(root.path(), &path).await.unwrap();
        tokio::fs::remove_dir(&path).await.unwrap();
        std::os::unix::fs::symlink(root.path(), &path).unwrap();
        assert!(literal_directory(root.path(), &path).await.is_err());
    }
}

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

    /// Resolves only a current, applied copy. Call again at admission with the term
    /// captured by the caller; a retired directory is never redirected to a new term.
    pub async fn repository_path(
        &self,
        repository: i64,
        uuid: uuid::Uuid,
        read_only: bool,
        expected_term: Option<i64>,
    ) -> Result<(PathBuf, Option<i64>), RepoError> {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.resolve_repository_path(repository, uuid, read_only, expected_term),
        )
        .await
        .map_err(|_| unavailable())?
    }

    async fn resolve_repository_path(
        &self,
        repository: i64,
        uuid: uuid::Uuid,
        read_only: bool,
        expected_term: Option<i64>,
    ) -> Result<(PathBuf, Option<i64>), RepoError> {
        let pool = self.database()?;
        let settings = codeyard_db_cluster::cluster::writes::write_settings(pool)
            .await
            .map_err(|_| unavailable())?;
        if settings.write_quorum.is_none() {
            if expected_term.is_some() {
                return Err(unavailable());
            }
            return Ok((
                crate::repositories::store::primary_path(&self.repos_path, uuid),
                None,
            ));
        }
        let view = codeyard_db_cluster::cluster::writes::recovery_view(pool, repository)
            .await
            .map_err(|_| unavailable())?;
        active_storage(pool, self.node_id, Some(&self.key.public_hex())).await?;
        if !current_copy(&view, self.node_id, read_only, expected_term) {
            return Err(unavailable());
        }
        let path = term_path(&self.repos_path, uuid, view.term);
        literal_directory(&self.repos_path, &path).await?;
        Ok((path, Some(view.term)))
    }

    pub async fn repository_store(
        &self,
        repository: &codeyard_db_core::models::Repository,
        read_only: bool,
    ) -> Result<crate::repositories::store::RepoStore, RepoError> {
        let uuid = crate::cluster::rpc::repository::repository_uuid(&repository.disk_path)?;
        let (path, _) = self
            .repository_path(repository.id, uuid, read_only, None)
            .await?;
        crate::repositories::store::RepoStore::on_context(
            path.to_str().ok_or_else(unavailable)?,
            repository.id,
            self,
        )
        .await
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
        let mut peers = self
            .peers
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let changed = peers.get(&id).is_none_or(|old| {
            old.public_key != peer.public_key
                || old.address != peer.address
                || old.target != peer.target
        });
        peers.insert(id, peer);
        drop(peers);
        if changed {
            self.note_dial(id, true);
        }
    }
}
