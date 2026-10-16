//! Files and sockets only the server's user may touch, for the OpenSSH gateway (Linux).
//! Native children use backend::process.
use super::validation::shell_safe_path;
use anyhow::{bail, ensure, Context, Result};
use std::fs::{DirBuilder, File, Metadata, OpenOptions};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::{timeout, Duration};

extern "C" {
    fn flock(fd: std::os::raw::c_int, operation: std::os::raw::c_int) -> std::os::raw::c_int;
}

pub(crate) fn effective_uid() -> Result<u32> {
    // Unlike UID environment variables or file ownership, peer_cred observes
    // this process's effective credentials, including setuid transitions.
    let (left, _right) = UnixStream::pair()?;
    Ok(left.peer_cred()?.uid())
}

/// A new temporary directory only this user can enter. `tempfile` applies the
/// process umask, which normally leaves directories readable by everyone, and
/// `bind` refuses a socket directory that is not private.
pub(crate) fn private_tempdir(prefix: &str) -> Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .prefix(prefix)
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?)
}

fn same_inode(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn ancestors(path: &Path, uid: u32) -> Result<()> {
    for ancestor in path.ancestors() {
        let meta = std::fs::symlink_metadata(ancestor)?;
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "unsafe path ancestor"
        );
        ensure!(
            meta.uid() == uid || meta.uid() == 0,
            "untrusted path ancestor owner"
        );
        // /tmp and /run/user's ancestors are not our directories to chmod. A
        // root-owned sticky ancestor is allowed; the immediate socket parent
        // is always private and no untrusted user can replace owned children.
        let sticky_root = meta.uid() == 0 && meta.mode() & 0o1000 != 0;
        ensure!(
            meta.mode() & 0o022 == 0 || sticky_root,
            "writable path ancestor"
        );
    }
    Ok(())
}

fn private_parent(socket: &Path, uid: u32, create: bool) -> Result<()> {
    shell_safe_path(socket)?;
    let parent = socket.parent().context("socket parent required")?;
    if create && !parent.exists() {
        ancestors(parent.parent().context("socket parent required")?, uid)?;
        // Only this one missing directory is created, never chmod existing dirs.
        DirBuilder::new().mode(0o700).create(parent)?;
    }
    ancestors(parent, uid)?;
    let meta = std::fs::symlink_metadata(parent)?;
    ensure!(
        meta.uid() == uid && meta.mode() & 0o077 == 0,
        "socket parent must be owned and private (0700)"
    );
    Ok(())
}

fn socket_metadata(path: &Path, uid: u32) -> Result<Metadata> {
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(
        meta.file_type().is_socket() && meta.uid() == uid && meta.mode() & 0o777 == 0o600,
        "unsafe existing socket path"
    );
    Ok(meta)
}

pub(crate) struct SocketGuard {
    path: PathBuf,
    inode: Metadata,
    // This owned 0600 advisory-lock inode is intentionally retained on disk.
    // Unlinking a lock while another process has it open creates split locks.
    _lock: File,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|meta| same_inode(&meta, &self.inode) && meta.file_type().is_socket())
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) async fn bind(socket: &Path, uid: u32) -> Result<(UnixListener, SocketGuard)> {
    private_parent(socket, uid, true)?;
    let mut lock_name = socket.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(0x20000)
        .open(PathBuf::from(lock_name))?; // O_NOFOLLOW
    let meta = lock.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == uid
            && meta.mode() & 0o777 == 0o600
            && meta.nlink() == 1
            && meta.len() == 0,
        "unsafe gateway lock"
    );
    // SAFETY: the owned fd is valid; Linux LOCK_EX|LOCK_NB does not retain pointers.
    if unsafe { flock(lock.as_raw_fd(), 2 | 4) } != 0 {
        bail!("gateway socket is already owned by another process");
    }
    match std::fs::symlink_metadata(socket) {
        Ok(_) => {
            let before = socket_metadata(socket, uid)?;
            match timeout(Duration::from_secs(2), UnixStream::connect(socket)).await {
                Ok(Err(error)) if error.kind() == ErrorKind::ConnectionRefused => {
                    let after = socket_metadata(socket, uid)?;
                    ensure!(
                        same_inode(&before, &after),
                        "socket changed during stale check"
                    );
                    std::fs::remove_file(socket)?;
                }
                _ => bail!("refusing to replace an active or uncertain socket"),
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(socket)?;
    let guard = SocketGuard {
        path: socket.to_owned(),
        inode: std::fs::symlink_metadata(socket)?,
        _lock: lock,
    };
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    socket_metadata(socket, uid)?;
    Ok((listener, guard))
}

pub(crate) async fn connect(socket: &Path) -> Result<UnixStream> {
    let uid = effective_uid()?;
    let owner = std::fs::symlink_metadata(socket)?.uid();
    ensure!(uid == 0 || owner == uid, "socket belongs to another user");
    private_parent(socket, owner, false)?;
    let before = socket_metadata(socket, owner)?;
    let stream = timeout(Duration::from_secs(5), UnixStream::connect(socket)).await??;
    let peer = stream.peer_cred()?.uid();
    ensure!(peer == owner || peer == 0, "untrusted broker peer");
    let after = socket_metadata(socket, owner)?;
    ensure!(same_inode(&before, &after), "socket changed during connect");
    Ok(stream)
}

pub(crate) fn trusted_executable(path: &Path, uid: u32) -> Result<PathBuf> {
    shell_safe_path(path)?;
    ancestors(path.parent().context("executable parent required")?, uid)?;
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file()
            && !meta.file_type().is_symlink()
            && (meta.uid() == uid || meta.uid() == 0)
            && meta.mode() & 0o022 == 0
            && meta.mode() & 0o111 != 0,
        "untrusted executable"
    );
    Ok(path.to_owned())
}

/// Git as found on PATH, followed through symbolic links (`/bin` is one on many systems)
/// so that the file and directories that are checked are the ones that run.
pub(crate) fn git_executable(uid: u32) -> Result<PathBuf> {
    let found = crate::backend::git::repository::process::executable()?;
    trusted_executable(&std::fs::canonicalize(found)?, uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn private_tempdir_is_closed_to_other_users() {
        let directory = private_tempdir("openssh-private-").unwrap();
        let mode = std::fs::metadata(directory.path()).unwrap().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[tokio::test]
    async fn socket_binding_refuses_unsafe_paths_and_preserves_active_socket() {
        let directory = private_tempdir("openssh-socket-").unwrap();
        let uid = effective_uid().unwrap();
        let socket = directory.path().join("broker.sock");
        let (listener, guard) = bind(&socket, uid).await.unwrap();
        assert_eq!(std::fs::metadata(&socket).unwrap().mode() & 0o777, 0o600);
        assert!(bind(&socket, uid).await.is_err());
        assert!(socket.exists());
        drop(listener);
        drop(guard);
        assert!(!socket.exists());
        std::fs::write(&socket, b"unrelated").unwrap();
        assert!(bind(&socket, uid).await.is_err());
        assert_eq!(std::fs::read(&socket).unwrap(), b"unrelated");
        std::fs::remove_file(&socket).unwrap();
        symlink(directory.path().join("unrelated"), &socket).unwrap();
        assert!(bind(&socket, uid).await.is_err());
        assert!(std::fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[tokio::test]
    async fn owned_cleanup_never_removes_a_replacement_path() {
        let directory = private_tempdir("openssh-replacement-").unwrap();
        let socket = directory.path().join("broker.sock");
        let (listener, guard) = bind(&socket, effective_uid().unwrap()).await.unwrap();
        std::fs::remove_file(&socket).unwrap();
        std::fs::write(&socket, b"replacement").unwrap();
        drop(listener);
        drop(guard);
        assert_eq!(std::fs::read(&socket).unwrap(), b"replacement");
    }

    #[tokio::test]
    async fn owned_stale_socket_can_be_reclaimed_without_chmodding_parent() {
        let directory = private_tempdir("openssh-stale-").unwrap();
        let socket = directory.path().join("broker.sock");
        let stale = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        drop(stale);
        let (listener, guard) = bind(&socket, effective_uid().unwrap()).await.unwrap();
        drop(listener);
        drop(guard);
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(bind(&socket, effective_uid().unwrap()).await.is_err());
        assert_eq!(
            std::fs::metadata(directory.path()).unwrap().mode() & 0o777,
            0o777
        );
    }
}
