//! Repository serialization. Ordinary operations use the process mutex and Git CAS;
//! branch provenance additionally uses an interprocess lease. Publication subprocesses
//! inherit that lease, so cancellation or a backend crash cannot release it before Git exits.
//! Readers of a directory share a reader guard, which replacing, moving or retiring the
//! directory takes exclusively.
use std::fs::File;
use std::future::Future;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use tokio::sync::{OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard};

static LOCKS: LazyLock<crate::backend::locks::KeyedMutex<PathBuf>> =
    LazyLock::new(Default::default);
static READERS: LazyLock<crate::backend::locks::KeyedRwLock<PathBuf>> =
    LazyLock::new(Default::default);

/// Receive sessions use canonical paths; API writes may retain a symlink alias. Resolve the
/// nearest existing ancestor, including before a destination directory exists, so creating
/// that directory does not change the lock selected through a symlinked parent.
async fn canonical(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() { path.to_owned() } else { std::env::current_dir().unwrap_or_default().join(path) };
    let mut ancestor = absolute.as_path();
    let mut tail = Vec::new();
    loop {
        if let Ok(mut canonical) = tokio::fs::canonicalize(ancestor).await {
            for component in tail.iter().rev() { canonical.push(component); }
            break canonical;
        }
        let Some(name) = ancestor.file_name() else { break absolute.clone(); };
        tail.push(name.to_owned());
        let Some(parent) = ancestor.parent() else { break absolute.clone(); };
        ancestor = parent;
    }
}

pub async fn lock(path: &Path) -> OwnedMutexGuard<()> {
    LOCKS.lock(canonical(path).await).await
}

/// Held for the whole of a read, alongside other readers and writers of the directory.
pub async fn read(path: &Path) -> OwnedRwLockReadGuard<()> {
    READERS.read(canonical(path).await).await
}

/// Waits for the directory's readers to finish and keeps new ones out, before it is replaced,
/// moved or retired. Take it after `lock`.
pub async fn exclusive(path: &Path) -> OwnedRwLockWriteGuard<()> {
    READERS.write(canonical(path).await).await
}

#[derive(Debug)]
pub(crate) struct PublicationGuard {
    _process: OwnedMutexGuard<()>,
    lease: Arc<PublicationLease>,
}

#[derive(Debug)]
pub(crate) struct PublicationLease {
    file: File,
}

tokio::task_local! { static PUBLICATION_FD: i32; }

impl PublicationGuard {
    pub fn scope<'a, T: 'a>(
        &'a self,
        work: impl Future<Output = T> + 'a,
    ) -> impl Future<Output = T> + 'a {
        self.lease.scope(work)
    }
}

impl PublicationLease {
    pub fn scope<'a, T: 'a>(
        &'a self,
        work: impl Future<Output = T> + 'a,
    ) -> impl Future<Output = T> + 'a {
        // Git publication futures can be large. Do not duplicate them in nested async scope
        // frames or inflate every enclosing store/RPC future (and its caller's stack).
        PUBLICATION_FD.scope(self.file.as_raw_fd(), Box::pin(work))
    }

    pub async fn acquire(self: &Arc<Self>, path: &Path) -> anyhow::Result<Arc<PublicationGuard>> {
        loop {
            // SAFETY: this live descriptor is retained by the lease and its inherited duplicates.
            if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if !matches!(
                error.raw_os_error(),
                Some(libc::EWOULDBLOCK) | Some(libc::EINTR)
            ) {
                return Err(error.into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // Wait for the OS lease before occupying the mutex needed by earlier post-receive work.
        let process = lock(path).await;
        Ok(Arc::new(PublicationGuard {
            _process: process,
            lease: self.clone(),
        }))
    }

    /// Only a validated post-receive hook may release a live child's lease: Git has already
    /// completed all ref publication then. A cancelled or failed pre-hook must not unlock it.
    pub fn published(&self) -> std::io::Result<()> {
        loop {
            // SAFETY: this descriptor is live. flock changes this operation's open file
            // description, shared with its child, not another receive's independent lease.
            if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) } == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error);
            }
        }
    }
}

/// Open before spawning receive-pack, but acquire only in pre-receive. Concurrent clients can
/// negotiate while the eventual lock is shared with the child through the same file description.
pub(crate) async fn open_publication(path: &Path) -> anyhow::Result<Arc<PublicationLease>> {
    let canonical = tokio::fs::canonicalize(path).await?;
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(canonical.join("codeyard-publication.lock"))
        .await?
        .into_std()
        .await;
    Ok(Arc::new(PublicationLease { file }))
}

/// All native branch publishers and provenance readers acquire this lease. Retain it in blocking
/// workers and scope publication subprocesses with it; never explicitly unlock on cancellation.
pub(crate) async fn publication(path: &Path) -> anyhow::Result<Arc<PublicationGuard>> {
    open_publication(path).await?.acquire(path).await
}

/// Attach only the selected lease to the child, never all the process's repository locks.
/// The command owns its duplicate even if spawning is deferred beyond the caller's lifetime.
pub(crate) fn inherit_publication(command: &mut std::process::Command) {
    let Ok(fd) = PUBLICATION_FD.try_with(|fd| *fd) else {
        return;
    };
    // SAFETY: scope keeps fd alive, and a successful fcntl returns a newly owned descriptor.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        let error = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EMFILE);
        // SAFETY: constructing the error needs no allocation or non-signal-safe operation.
        unsafe {
            command.pre_exec(move || Err(std::io::Error::from_raw_os_error(error)));
        }
        return;
    }
    // SAFETY: this newly duplicated descriptor has exactly one Rust owner.
    let owned = unsafe { OwnedFd::from_raw_fd(duplicate) };
    // SAFETY: the child only makes async-signal-safe fcntl calls before exec.
    unsafe {
        command.pre_exec(move || {
            let fd = owned.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publication_scope_does_not_embed_the_publication_future() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repo.git");
        git2::Repository::init_bare(&path).unwrap();
        let held = publication(&path).await.unwrap();
        let payload = [7_u8; 32 * 1024];
        let work = async move {
            tokio::task::yield_now().await;
            std::hint::black_box(payload).len()
        };
        assert!(std::mem::size_of_val(&work) >= 32 * 1024);
        let scoped = held.scope(work);
        assert!(std::mem::size_of_val(&scoped) < 512);
        assert_eq!(scoped.await, 32 * 1024);
    }

    #[tokio::test]
    async fn publication_subprocess_keeps_the_interprocess_lease_after_caller_drop() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repo.git");
        git2::Repository::init_bare(&path).unwrap();
        let lease = open_publication(&path).await.unwrap();
        let mut child = lease
            .scope(async {
                let mut command = super::super::process::git_command().unwrap();
                command.current_dir(&path).args(["hash-object", "--stdin"]);
                command.spawn().unwrap()
            })
            .await;
        // As in receive-pack, inherit the description before acquiring its lock in the hook.
        let held = lease.acquire(&path).await.unwrap();
        drop(lease);
        drop(held);
        let waiting_path = path.clone();
        let waiting = tokio::spawn(async move { publication(&waiting_path).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiting.is_finished());
        // The waiting publisher must not occupy the mutex needed by the receive child's
        // post-receive work. Otherwise neither it nor the child can finish.
        drop(
            tokio::time::timeout(std::time::Duration::from_millis(100), lock(&path))
                .await
                .unwrap(),
        );
        drop(child.stdin.take());
        assert!(child.wait().await.unwrap().success());
        tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn reported_publication_releases_the_lease_before_post_receive_child_exits() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repo.git");
        git2::Repository::init_bare(&path).unwrap();
        let lease = open_publication(&path).await.unwrap();
        let mut child = lease
            .scope(async {
                let mut command = super::super::process::git_command().unwrap();
                command.current_dir(&path).args(["hash-object", "--stdin"]);
                command.spawn().unwrap()
            })
            .await;
        let held = lease.acquire(&path).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), publication(&path))
                .await
                .is_err()
        );
        lease.published().unwrap();
        drop(held);
        let next = tokio::time::timeout(std::time::Duration::from_secs(2), publication(&path))
            .await
            .unwrap()
            .unwrap();
        assert!(child.try_wait().unwrap().is_none());
        drop(child.stdin.take());
        assert!(child.wait().await.unwrap().success());
        // Closing the earlier child's independent description cannot release this new lease.
        drop(lease);
        let probe = open_publication(&path).await.unwrap();
        // SAFETY: the probe owns a separate live description of this same lock file.
        assert_eq!(
            unsafe { libc::flock(probe.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EWOULDBLOCK)
        );
        drop(next);
    }

    #[tokio::test]
    async fn blocking_publication_worker_keeps_the_lease_when_its_caller_disappears() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repo.git");
        git2::Repository::init_bare(&path).unwrap();
        let held = publication(&path).await.unwrap();
        let worker_held = held.clone();
        let (ready, waiting) = tokio::sync::oneshot::channel();
        let (finish, finished) = std::sync::mpsc::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let _held = worker_held;
            ready.send(()).unwrap();
            finished.recv().unwrap();
        });
        waiting.await.unwrap();
        drop(held);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), publication(&path))
                .await
                .is_err()
        );
        finish.send(()).unwrap();
        worker.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), publication(&path))
            .await
            .unwrap()
            .unwrap();
    }

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
