//! Files and sockets only the server's user may touch, for the OpenSSH gateway (Linux).
//! Native children use backend::process.
use anyhow::bail;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use codeyard_repos::validation::shell_safe_path;
use std::fs::DirBuilder;
use std::fs::File;
use std::fs::Metadata;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio::time::Duration;

pub fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

fn same_inode(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// The server's private state directory, once trusted by `trust_state_dir`.
static STATE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Makes `state` the top of every ancestor check below it, as OpenSSH's StrictModes stops at
/// the home directory. It must be owned by this user and closed to others (0700, as the server
/// creates it). Where it lives is the operator's choice, as for the database and node keys it
/// holds; a Kubernetes volume, for one, is a world-writable directory without the sticky bit.
pub fn trust_state_dir(state: &Path) -> Result<()> {
    let state = std::fs::canonicalize(state)?;
    let meta = std::fs::symlink_metadata(&state)?;
    ensure!(
        meta.is_dir() && meta.uid() == effective_uid() && meta.mode() & 0o077 == 0,
        "state directory must be owned and private (0700)"
    );
    ensure!(
        *STATE_DIR.get_or_init(|| state.clone()) == state,
        "another state directory is already trusted"
    );
    Ok(())
}

/// The trusted state directory, for receive hooks started by this server.
pub fn trusted_state_dir() -> Option<&'static Path> {
    STATE_DIR.get().map(PathBuf::as_path)
}

/// Where Git receives make their private socket directories: `run` in the trusted state
/// directory, or the system temporary directory when none is trusted (commands and tests).
pub fn run_dir() -> Option<PathBuf> {
    STATE_DIR.get().map(|state| state.join("run"))
}

/// The longest state directory that leaves room for `run/git-receive-XXXXXX/broker.sock` in
/// a Unix socket address (107 bytes and a NUL).
const LONGEST_STATE_DIR: usize = 107 - "/run/git-receive-XXXXXX/broker.sock".len();

/// Server start: trusts the state directory and empties its `run` directory, which can only
/// hold the sockets of an earlier server that was killed. The caller holds the state
/// directory lock, so no other server uses them. A state directory whose path cannot hold
/// a socket or appear in a hook script leaves the sockets in the system temporary directory.
pub fn prepare_run_dir(state: &Path) -> Result<()> {
    let state = std::fs::canonicalize(state)?;
    if state.as_os_str().len() > LONGEST_STATE_DIR || shell_safe_path(&state).is_err() {
        tracing::warn!(
            state = %state.display(),
            "the state directory path is too long or has unusual characters; Git receive sockets stay in the system temporary directory"
        );
        return Ok(());
    }
    trust_state_dir(&state)?;
    let run = run_dir().context("state directory required")?;
    match std::fs::remove_dir_all(&run) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    DirBuilder::new().mode(0o700).create(&run)?;
    Ok(())
}

/// Checks `path` and its parents up to the trusted state directory, or up to `/`.
pub fn ancestors(path: &Path, uid: u32) -> Result<()> {
    ancestors_below(path, uid, trusted_state_dir())
}

fn ancestors_below(path: &Path, uid: u32, state: Option<&Path>) -> Result<()> {
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
        if state.is_some_and(|state| state == ancestor)
            && meta.uid() == uid
            && meta.mode() & 0o077 == 0
        {
            break;
        }
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

pub struct SocketGuard {
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

pub async fn bind(socket: &Path, uid: u32) -> Result<(UnixListener, SocketGuard)> {
    private_parent(socket, uid, true)?;
    let mut lock_name = socket.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(PathBuf::from(lock_name))?;
    let meta = lock.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == uid
            && meta.mode() & 0o777 == 0o600
            && meta.nlink() == 1
            && meta.len() == 0,
        "unsafe gateway lock"
    );
    // SAFETY: the owned fd is valid and flock keeps no pointers.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("gateway socket is already owned by another process");
    }
    match std::fs::symlink_metadata(socket) {
        Ok(_) => {
            let stale_metadata = || -> Result<Metadata> {
                let meta = std::fs::symlink_metadata(socket)?;
                ensure!(
                    meta.file_type().is_socket() && meta.uid() == uid,
                    "unsafe existing socket path"
                );
                Ok(meta)
            };
            let before = stale_metadata()?;
            match timeout(Duration::from_secs(2), UnixStream::connect(socket)).await {
                Ok(Err(error)) if error.kind() == ErrorKind::ConnectionRefused => {
                    let after = stale_metadata()?;
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

pub async fn connect(socket: &Path) -> Result<UnixStream> {
    let uid = effective_uid();
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

pub fn trusted_executable(path: &Path, uid: u32) -> Result<PathBuf> {
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
pub fn git_executable(uid: u32) -> Result<PathBuf> {
    let found = codeyard_git::repository::process::executable()?;
    trusted_executable(&std::fs::canonicalize(found)?, uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn private_tempdir_is_closed_to_other_users() {
        let directory = codeyard_runner::fs::private_tempdir("openssh-private-", None).unwrap();
        let mode = std::fs::metadata(directory.path()).unwrap().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[tokio::test]
    async fn socket_binding_refuses_unsafe_paths_and_preserves_active_socket() {
        let directory = codeyard_runner::fs::private_tempdir("openssh-socket-", None).unwrap();
        let uid = effective_uid();
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

    #[test]
    fn a_private_state_directory_is_trusted_under_a_shared_volume() {
        // A Kubernetes emptyDir is world-writable without the sticky bit.
        let directory = codeyard_runner::fs::private_tempdir("openssh-volume-", None).unwrap();
        let volume = directory.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        std::fs::set_permissions(&volume, std::fs::Permissions::from_mode(0o777)).unwrap();
        let state = volume.join("state");
        DirBuilder::new().mode(0o700).create(&state).unwrap();
        let uid = effective_uid();
        let run = state.join("run");
        DirBuilder::new().mode(0o700).create(&run).unwrap();
        assert!(ancestors_below(&run, uid, None).is_err());
        ancestors_below(&run, uid, Some(&state)).unwrap();
        assert!(trust_state_dir(&volume).is_err());

        let receive = codeyard_runner::fs::private_tempdir("git-receive-", Some(&run)).unwrap();
        let socket = receive.path().join("broker.sock");
        assert!(ancestors_below(&socket, uid, None).is_err());
        ancestors_below(receive.path(), uid, Some(&state)).unwrap();
        assert_eq!(
            std::fs::metadata(receive.path()).unwrap().mode() & 0o777,
            0o700
        );

        // The state directory itself must stay private to be the top of the check.
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(ancestors_below(&run, uid, Some(&state)).is_err());
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Everything below it is still checked.
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(ancestors_below(receive.path(), uid, Some(&state)).is_err());
    }

    #[tokio::test]
    async fn owned_cleanup_never_removes_a_replacement_path() {
        let directory = codeyard_runner::fs::private_tempdir("openssh-replacement-", None).unwrap();
        let socket = directory.path().join("broker.sock");
        let (listener, guard) = bind(&socket, effective_uid()).await.unwrap();
        std::fs::remove_file(&socket).unwrap();
        std::fs::write(&socket, b"replacement").unwrap();
        drop(listener);
        drop(guard);
        assert_eq!(std::fs::read(&socket).unwrap(), b"replacement");
    }

    #[tokio::test]
    async fn owned_stale_socket_can_be_reclaimed_without_chmodding_parent() {
        let directory = codeyard_runner::fs::private_tempdir("openssh-stale-", None).unwrap();
        let socket = directory.path().join("broker.sock");
        let stale = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        drop(stale);
        let (listener, guard) = bind(&socket, effective_uid()).await.unwrap();
        drop(listener);
        drop(guard);
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(bind(&socket, effective_uid()).await.is_err());
        assert_eq!(
            std::fs::metadata(directory.path()).unwrap().mode() & 0o777,
            0o777
        );
    }
}
