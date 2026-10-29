//! Process-lifetime filesystem ownership.
use anyhow::{ensure, Context, Result};
use std::{fs::File, os::fd::AsRawFd, path::Path};

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
