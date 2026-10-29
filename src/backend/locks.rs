//! Filesystem ownership and evicting keyed process mutexes.
use anyhow::{ensure, Context, Result};
use std::{collections::HashMap, fs::File, hash::Hash, os::fd::AsRawFd, path::Path, sync::{Arc, Mutex}};

pub(crate) fn exclusive_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true)
        .custom_flags(libc::O_NOFOLLOW).open(path)
        .with_context(|| format!("open ownership lock {}", path.display()))?;
    // SAFETY: the descriptor is owned by the returned file and flock retains no pointers.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        ensure!(error.kind() != std::io::ErrorKind::WouldBlock,
            "another codeyard process owns {}", path.display());
        return Err(error.into());
    }
    Ok(file)
}

pub(crate) struct KeyedMutex<K>(Mutex<HashMap<K, Arc<tokio::sync::Mutex<()>>>>);

impl<K> Default for KeyedMutex<K> {
    fn default() -> Self { Self(Mutex::new(HashMap::new())) }
}

impl<K: Eq + Hash> KeyedMutex<K> {
    pub async fn lock(&self, key: K) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = {
            let mut locks = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
            locks.retain(|_, mutex| Arc::strong_count(mutex) > 1);
            locks.entry(key).or_default().clone()
        };
        mutex.lock_owned().await
    }
}
