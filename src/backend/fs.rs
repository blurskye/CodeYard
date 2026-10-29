//! Host-owned persistent state directories and their process-lifetime exclusion.
use std::path::Path;

pub(crate) fn private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The same lock protects serving and persistent runner work-folder changes.
pub(crate) fn lock_state_dir(path: &Path) -> anyhow::Result<std::fs::File> {
    private_dir(path)?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true)
        .open(path.join("serve.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!("another codeyard server is using the state directory {}", path.display()),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}
