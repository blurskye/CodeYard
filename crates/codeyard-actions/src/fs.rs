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
