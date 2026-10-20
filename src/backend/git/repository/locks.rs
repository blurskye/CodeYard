//! Per-repository write lock for writes made by this server process (editor
//! saves, merges, branch/tag changes). Correctness across processes and nodes
//! comes from `update-ref` checking the old value; the lock just avoids
//! pointless conflicts between requests on the same node.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use tokio::sync::OwnedMutexGuard;

static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

pub async fn lock(path: &Path) -> OwnedMutexGuard<()> {
    // Receive sessions use canonical paths; API writes may retain a symlink alias.
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mutex = {
        let mut locks = LOCKS.lock().unwrap_or_else(|poison| poison.into_inner());
        locks.retain(|_, mutex| Arc::strong_count(mutex) > 1);
        locks.entry(path).or_default().clone()
    };
    mutex.lock_owned().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn symlink_aliases_share_the_publication_lock() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repo.git");
        std::fs::create_dir(&path).unwrap();
        let alias = root.path().join("alias.git");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let held = lock(&path).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), lock(&alias))
                .await
                .is_err()
        );
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(1), lock(&alias))
            .await
            .unwrap();
    }
}
