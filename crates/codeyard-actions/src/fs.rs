//! Private temporary storage shared by execution and server credential producers.
use std::path::Path;

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
