//! Filesystem ownership and evicting keyed process locks.
use anyhow::{ensure, Context, Result};
use std::{
    collections::HashMap,
    fs::File,
    hash::Hash,
    os::fd::AsRawFd,
    path::Path,
    sync::{Arc, Mutex},
};

pub fn exclusive_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open ownership lock {}", path.display()))?;
    // SAFETY: the descriptor is owned by the returned file and flock retains no pointers.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        ensure!(
            error.kind() != std::io::ErrorKind::WouldBlock,
            "another codeyard process owns {}",
            path.display()
        );
        return Err(error.into());
    }
    Ok(file)
}

/// One process lock per key; a key's entry is dropped once no guard or waiter holds it.
pub struct Keyed<K, L>(Mutex<HashMap<K, Arc<L>>>);

pub type KeyedMutex<K> = Keyed<K, tokio::sync::Mutex<()>>;
pub type KeyedRwLock<K> = Keyed<K, tokio::sync::RwLock<()>>;

impl<K, L> Default for Keyed<K, L> {
    fn default() -> Self {
        Self(Mutex::new(HashMap::new()))
    }
}

impl<K: Eq + Hash, L: Default> Keyed<K, L> {
    fn entry(&self, key: K) -> Arc<L> {
        let mut locks = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        locks.entry(key).or_default().clone()
    }
}

impl<K: Eq + Hash> KeyedMutex<K> {
    pub async fn lock(&self, key: K) -> tokio::sync::OwnedMutexGuard<()> {
        self.entry(key).lock_owned().await
    }
}

impl<K: Eq + Hash> KeyedRwLock<K> {
    pub async fn read(&self, key: K) -> tokio::sync::OwnedRwLockReadGuard<()> {
        self.entry(key).read_owned().await
    }

    pub async fn write(&self, key: K) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.entry(key).write_owned().await
    }
}
