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
    let mutex = {
        let mut locks = LOCKS.lock().unwrap_or_else(|poison| poison.into_inner());
        locks.retain(|_, mutex| Arc::strong_count(mutex) > 1);
        locks.entry(path.to_path_buf()).or_default().clone()
    };
    mutex.lock_owned().await
}
