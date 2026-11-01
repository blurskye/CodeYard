//! Host-owned persistent state directories and their process-lifetime exclusion.
use std::path::Path;

/// Creates `path` and its missing parents, syncing each new directory and its parent so
/// the directories survive a power loss.
pub(crate) fn durable_dir(path: &Path) -> std::io::Result<()> {
    create_durable(path, std::fs::DirBuilder::new().recursive(true))
}

/// A durable directory only this user can enter.
pub(crate) fn private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    create_durable(path, &builder)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn create_durable(path: &Path, builder: &std::fs::DirBuilder) -> std::io::Result<()> {
    let mut created = Vec::new();
    let mut missing = path;
    while !missing.exists() {
        created.push(missing);
        match missing.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => missing = parent,
            _ => break,
        }
    }
    builder.create(path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("cannot create {}: {error}", path.display()),
        )
    })?;
    for made in created.into_iter().rev() {
        std::fs::File::open(made)?.sync_all()?;
        let parent = made
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// The same lock protects serving and persistent runner work-folder changes.
pub(crate) fn lock_state_dir(path: &Path) -> anyhow::Result<std::fs::File> {
    private_dir(path)?;
    crate::backend::locks::exclusive_file(&path.join("serve.lock"))
}
