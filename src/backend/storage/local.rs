//! Uploaded files stored as plain files under `<state_dir>/objects`.
use super::{media_signature_matches, valid_key, StorageError, Upload, MAX_OBJECT_BYTES};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Uploads are written here before they are renamed into place. Keys never contain a dot
/// (see `valid_key`), so this directory never holds an object.
const STAGING_DIR: &str = ".staging";
/// Temporary files this app writes into its staging directories start with this prefix, so
/// the cleanup removes only files it made.
pub const TEMP_PREFIX: &str = "codeyard-tmp-";
/// A temporary file that nothing has written to for this long was left by a crash.
pub const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

pub struct LocalStore {
    root: PathBuf,
}

impl LocalStore {
    pub async fn open(root: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(root)
            .await
            .with_context(|| format!("create object directory {}", root.display()))?;
        let root = tokio::fs::canonicalize(root).await?;
        sync_parents(&root).await?;
        Ok(Self { root })
    }

    fn path(&self, key: &str) -> Result<PathBuf, StorageError> {
        if !valid_key(key) {
            return Err(StorageError::Invalid);
        }
        Ok(self.root.join(key))
    }

    /// Streams into a temporary file and renames it into place only after the
    /// size, checksum and file signature check out.
    pub async fn put(
        &self,
        upload: &Upload<'_>,
        mut reader: impl AsyncRead + Unpin,
    ) -> Result<(), StorageError> {
        if !(1..=MAX_OBJECT_BYTES).contains(&upload.bytes) {
            return Err(StorageError::Invalid);
        }
        let path = self.path(upload.key)?;
        let parent = path.parent().ok_or(StorageError::Invalid)?;
        tokio::fs::create_dir_all(parent).await?;
        // Partial files share one directory, so the cleanup finds the ones a crash leaves.
        let staging = self.root.join(STAGING_DIR);
        tokio::fs::create_dir_all(&staging).await?;
        let temporary = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .tempfile_in(staging)?;
        let mut output = tokio::fs::File::from_std(temporary.reopen()?);
        let mut buffer = vec![0u8; 64 * 1024];
        let mut hash = Sha256::new();
        let mut prefix = Vec::with_capacity(16);
        let mut count = 0i64;
        loop {
            // Read one byte past the declared size so oversized bodies are caught.
            let wanted = ((upload.bytes - count + 1) as usize).min(buffer.len());
            let read = reader
                .read(&mut buffer[..wanted])
                .await
                .map_err(|_| StorageError::Rejected)?;
            if read == 0 {
                break;
            }
            count += read as i64;
            if count > upload.bytes {
                return Err(StorageError::Rejected);
            }
            let take = (16 - prefix.len()).min(read);
            prefix.extend_from_slice(&buffer[..take]);
            hash.update(&buffer[..read]);
            output.write_all(&buffer[..read]).await?;
        }
        if count != upload.bytes
            || hex::encode(hash.finalize()) != upload.sha256_hex
            || !media_signature_matches(upload.content_type, &prefix)
        {
            return Err(StorageError::Rejected);
        }
        output.flush().await?;
        output.sync_all().await?;
        temporary
            .persist(&path)
            .map_err(|_| StorageError::Unavailable)?;
        sync_parents(parent).await?;
        Ok(())
    }

    /// Bytes are checked on the way in, so existence and size are enough here.
    pub async fn verify(&self, upload: &Upload<'_>) -> Result<(), StorageError> {
        let metadata = tokio::fs::metadata(self.path(upload.key)?)
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    StorageError::NotFound
                } else {
                    StorageError::Unavailable
                }
            })?;
        if !metadata.is_file() || metadata.len() != upload.bytes as u64 {
            return Err(StorageError::Rejected);
        }
        Ok(())
    }

    pub async fn open_file(&self, key: &str) -> Result<(tokio::fs::File, u64), StorageError> {
        let file = tokio::fs::File::open(self.path(key)?).await?;
        let bytes = file.metadata().await?.len();
        Ok((file, bytes))
    }

    pub async fn delete(&self, key: &str) -> Result<(), StorageError> {
        match tokio::fs::remove_file(self.path(key)?).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }

    /// Removes staged uploads that a crash left behind.
    pub async fn remove_stale_staging(&self) -> std::io::Result<usize> {
        let staging = self.root.join(STAGING_DIR);
        remove_stale_temporaries(&staging, STALE_AFTER).await
    }
}

/// Persist the rename and any parent directories that were just created.
async fn sync_parents(path: &Path) -> std::io::Result<()> {
    for directory in path.ancestors() {
        tokio::fs::File::open(directory).await?.sync_all().await?;
    }
    Ok(())
}

/// Removes files in `dir` whose names start with `TEMP_PREFIX` and that nothing has written
/// to for `older_than`. A file being written has a fresh modification time. A file that its
/// owner removes meanwhile is skipped, because that is not an error.
pub async fn remove_stale_temporaries(dir: &Path, older_than: Duration) -> std::io::Result<usize> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(TEMP_PREFIX) {
            continue;
        }
        let metadata = match entry.metadata().await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let idle = metadata.modified()?.elapsed().unwrap_or_default();
        if idle < older_than {
            continue;
        }
        match tokio::fs::remove_file(entry.path()).await {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn upload<'a>(key: &'a str, bytes: &[u8], sha: &'a str) -> Upload<'a> {
        Upload {
            key,
            bytes: bytes.len() as i64,
            content_type: "image/png",
            sha256_hex: sha,
        }
    }

    #[tokio::test]
    async fn put_checks_size_checksum_and_signature() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalStore::open(root.path()).await.unwrap();
        let png = b"\x89PNG\r\n\x1a\nrest";
        let sha = hex::encode(Sha256::digest(png));
        let key = "uploads/one";
        store.put(&upload(key, png, &sha), &png[..]).await.unwrap();
        store.verify(&upload(key, png, &sha)).await.unwrap();
        let (_, bytes) = store.open_file(key).await.unwrap();
        assert_eq!(bytes, png.len() as u64);

        let fake = b"<html>not png";
        let fake_sha = hex::encode(Sha256::digest(fake));
        assert!(store
            .put(&upload("uploads/two", fake, &fake_sha), &fake[..])
            .await
            .is_err());
        assert!(store
            .put(&upload("uploads/three", png, &fake_sha), &png[..])
            .await
            .is_err());
        let mut short = upload("uploads/four", png, &sha);
        short.bytes -= 1;
        assert!(store.put(&short, &png[..]).await.is_err());
        assert!(store
            .verify(&upload("uploads/two", fake, &fake_sha))
            .await
            .is_err());

        store.delete(key).await.unwrap();
        store.delete(key).await.unwrap();
        assert!(store.open_file(key).await.is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn final_write_errors_do_not_publish_truncated_objects() {
        const CHILD: &str = "CODEYARD_UPLOAD_WRITE_ERROR_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "backend::storage::local::tests::final_write_errors_do_not_publish_truncated_objects", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let store = LocalStore::open(root.path()).await.unwrap();
            let png = b"\x89PNG\r\n\x1a\nrest";
            let sha = hex::encode(Sha256::digest(png));
            // Isolated child: force the final asynchronous file write to fail.
            unsafe {
                libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                let limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
            }
            assert!(store
                .put(&upload("uploads/full", png, &sha), &png[..])
                .await
                .is_err());
            assert!(!root.path().join("uploads/full").exists());
        });
    }

    #[tokio::test]
    async fn published_uploads_are_complete_when_parent_directories_are_new() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalStore::open(&root.path().join("new/objects"))
            .await
            .unwrap();
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(vec![b'x'; 256 * 1024]);
        let sha = hex::encode(Sha256::digest(&png));
        store
            .put(&upload("uploads/new", &png, &sha), png.as_slice())
            .await
            .unwrap();
        let (mut file, size) = store.open_file("uploads/new").await.unwrap();
        assert_eq!(size, png.len() as u64);
        let mut read = Vec::new();
        file.read_to_end(&mut read).await.unwrap();
        assert_eq!(read, png);
    }

    #[tokio::test]
    async fn staging_sweep_removes_only_old_temporary_files() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalStore::open(root.path()).await.unwrap();
        let staging = root.path().join(STAGING_DIR);
        tokio::fs::create_dir_all(&staging).await.unwrap();
        let aged = staging.join(format!("{TEMP_PREFIX}aged"));
        let fresh = staging.join(format!("{TEMP_PREFIX}fresh"));
        let unrelated = staging.join("notes.txt");
        for path in [&fresh, &aged, &unrelated] {
            std::fs::write(path, b"partial upload").unwrap();
        }
        let old = SystemTime::now() - STALE_AFTER * 2;
        for path in [&aged, &unrelated] {
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(old).unwrap();
        }

        assert_eq!(store.remove_stale_staging().await.unwrap(), 1);
        assert!(!aged.exists());
        assert!(fresh.exists());
        assert!(unrelated.exists());
    }
}
