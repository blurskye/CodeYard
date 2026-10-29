//! Private temporary storage shared by execution and server credential producers.
use std::path::Path;

/// Replace the directory entry, never truncate an existing workflow-controlled symlink.
pub fn replace_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !std::fs::symlink_metadata(parent)?.is_dir() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Snapshot an upload tree through descriptor-relative, no-follow opens. The private copy
/// remains owned by the caller until its transfer completes; SCP never reads mutable sources.
#[cfg(target_os = "linux")]
pub fn transfer_snapshot(source: &Path) -> anyhow::Result<tempfile::TempDir> {
    use std::{ffi::CString, fs::{File, OpenOptions}, io::{Read, Write}, os::{fd::{AsRawFd, FromRawFd}, unix::{ffi::OsStrExt, fs::{OpenOptionsExt, PermissionsExt}}}};
    fn copy(directory: File, destination: &Path, depth: usize, entries: &mut usize, bytes: &mut u64) -> anyhow::Result<()> {
        anyhow::ensure!(depth <= 128, "Upload tree is too deeply nested");
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
            let name = entry?.file_name();
            *entries += 1;
            anyhow::ensure!(*entries <= 100_000, "Upload tree contains too many entries");
            let c_name = CString::new(name.as_bytes())?;
            // SAFETY: directory and name remain valid through this descriptor-relative open.
            let fd = unsafe { libc::openat(directory.as_raw_fd(), c_name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK) };
            if fd < 0 { return Err(std::io::Error::last_os_error().into()); }
            let mut input = unsafe { File::from_raw_fd(fd) };
            let metadata = input.metadata()?;
            let path = destination.join(&name);
            if metadata.is_dir() {
                std::fs::DirBuilder::new().create(&path)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
                copy(input, &path, depth + 1, entries, bytes)?;
            } else {
                anyhow::ensure!(metadata.is_file(), "Upload tree must contain only regular files and directories");
                let mut output = OpenOptions::new().write(true).create_new(true)
                    .mode(0o600 | (metadata.permissions().mode() & 0o100)).open(path)?;
                let mut buffer = [0; 64 * 1024];
                loop {
                    let read = input.read(&mut buffer)?;
                    if read == 0 { break; }
                    *bytes = bytes.checked_add(read as u64).ok_or_else(|| anyhow::anyhow!("Upload snapshot is too large"))?;
                    anyhow::ensure!(*bytes <= 16 * 1024 * 1024 * 1024, "Upload snapshot exceeds 16 GiB");
                    output.write_all(&buffer[..read])?;
                }
            }
        }
        Ok(())
    }
    let source = OpenOptions::new().read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK).open(source)?;
    let snapshot = private_tempdir("codeyard-transfer-", None)?;
    copy(source, snapshot.path(), 0, &mut 0, &mut 0)?;
    Ok(snapshot)
}

#[cfg(not(target_os = "linux"))]
pub fn transfer_snapshot(_source: &Path) -> anyhow::Result<tempfile::TempDir> {
    anyhow::bail!("No-follow VM upload snapshots require a Linux host")
}

pub fn private_tempdir(prefix: &str, parent: Option<&Path>) -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    match parent {
        Some(parent) => builder.tempdir_in(parent),
        None => builder.tempdir(),
    }
}
