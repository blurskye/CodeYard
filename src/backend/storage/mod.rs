//! Where uploaded files live: a directory under the state directory, or an
//! S3-compatible bucket when the S3_* environment variables are set.
pub mod local;
pub mod s3;

use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::sync::{OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};

pub const MAX_OBJECT_BYTES: i64 = 100 * 1024 * 1024;
/// Concurrent uploads, confirmations and deletes this node performs. More are refused as
/// busy, not queued.
const IO_SLOTS: usize = 32;
/// Concurrent local file downloads; more are refused as busy, not queued, so
/// slow readers can't hold up uploads.
const DOWNLOAD_SLOTS: usize = 64;
/// Picture forms read at once. Each may buffer up to 2 MiB before it is stored, so this
/// bounds the memory that slow senders can hold.
const PICTURE_SLOTS: usize = 8;

/// What a client declared about an upload; checked when the bytes arrive.
pub struct Upload<'a> {
    pub key: &'a str,
    pub bytes: i64,
    pub content_type: &'a str,
    pub sha256_hex: &'a str,
}

/// Where and how the client should send the bytes.
pub struct UploadTarget {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    /// True when the client uploads straight to S3 rather than to this server.
    pub direct: bool,
}

/// Byte transport failures, independent of HTTP status or account policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    #[error("Invalid object declaration")]
    Invalid,
    #[error("Object bytes do not match the declaration")]
    Rejected,
    #[error("Object has not arrived")]
    NotFound,
    #[error("Object storage unavailable")]
    Unavailable,
}

impl From<std::io::Error> for StorageError {
    fn from(_: std::io::Error) -> Self {
        Self::Unavailable
    }
}

pub enum Download {
    Redirect(String),
    Stream { file: tokio::fs::File, bytes: u64 },
}

enum Backend {
    Local(local::LocalStore),
    S3(s3::S3Store),
}

pub struct ObjectStore {
    backend: Backend,
    io: Arc<Semaphore>,
    downloads: Arc<Semaphore>,
    pictures: Arc<Semaphore>,
    file_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ObjectStore {
    pub async fn open(state_dir: &Path, s3: Option<s3::S3Config>) -> Result<Self> {
        let backend = match s3 {
            Some(config) => Backend::S3(s3::S3Store::open(config).await?),
            None => Backend::Local(local::LocalStore::open(&state_dir.join("objects")).await?),
        };
        Ok(Self {
            backend,
            io: Arc::new(Semaphore::new(IO_SLOTS)),
            downloads: Arc::new(Semaphore::new(DOWNLOAD_SLOTS)),
            pictures: Arc::new(Semaphore::new(PICTURE_SLOTS)),
            file_locks: Mutex::new(HashMap::new()),
        })
    }

    /// Start-up check for S3: waits for the store to answer (compose starts it in parallel)
    /// and creates the bucket on first start. Local storage has nothing to prepare.
    pub async fn prepare(&self) -> Result<()> {
        let Backend::S3(store) = &self.backend else {
            return Ok(());
        };
        let mut attempt = 1;
        loop {
            match store.ensure_bucket().await {
                Ok(()) => return Ok(()),
                Err(_) if attempt < 10 => {
                    tracing::warn!(attempt, "S3 bucket preparation unavailable");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    attempt += 1;
                }
                Err(_) => anyhow::bail!("S3 bucket preparation failed"),
            }
        }
    }

    pub fn kind(&self) -> &'static str {
        match self.backend {
            Backend::Local(_) => "local",
            Backend::S3(_) => "s3",
        }
    }

    /// The bucket for the admin page. Local files have no location to show.
    pub fn location(&self) -> Option<String> {
        match &self.backend {
            Backend::Local(_) => None,
            Backend::S3(store) => Some(store.bucket().to_owned()),
        }
    }

    /// Serializes one file's local PUT, confirmation, deletion and expiry boundary.
    /// Different object keys retain independent locks.
    pub async fn lock_file(&self, key: &str) -> Result<OwnedMutexGuard<()>, StorageError> {
        if !valid_key(key) {
            return Err(StorageError::Invalid);
        }
        let lock = {
            let mut locks = self
                .file_locks
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
            locks.entry(key.to_owned()).or_default().clone()
        };
        Ok(lock.lock_owned().await)
    }

    /// An I/O slot, or `Unavailable` at once when every slot is busy. Nothing waits for a
    /// slot: the caller reports the busy state and the client retries.
    pub fn try_acquire_io(&self) -> Result<OwnedSemaphorePermit, StorageError> {
        self.io
            .clone()
            .try_acquire_owned()
            .map_err(|_| StorageError::Unavailable)
    }

    /// A download slot, or `None` when this node is serving too many already.
    pub fn try_acquire_download(&self) -> Option<OwnedSemaphorePermit> {
        self.downloads.clone().try_acquire_owned().ok()
    }

    /// A slot for reading one picture form, or `None` when this node is reading as many.
    pub fn try_acquire_picture(&self) -> Option<OwnedSemaphorePermit> {
        self.pictures.clone().try_acquire_owned().ok()
    }

    /// For local storage the client uploads to `local_url` on this server.
    pub async fn upload_target(
        &self,
        upload: &Upload<'_>,
        local_url: String,
        expires_in: Duration,
    ) -> Result<UploadTarget, StorageError> {
        match &self.backend {
            Backend::Local(_) => Ok(UploadTarget {
                url: local_url,
                headers: BTreeMap::from([
                    ("content-type".into(), upload.content_type.into()),
                    ("content-length".into(), upload.bytes.to_string()),
                ]),
                direct: false,
            }),
            Backend::S3(store) => {
                let signed = store
                    .presigned_put(upload, expires_in)
                    .await
                    .map_err(|_| StorageError::Unavailable)?;
                Ok(UploadTarget {
                    url: signed.url,
                    headers: signed.headers,
                    direct: true,
                })
            }
        }
    }

    /// Stores bytes sent to this server (local storage only), checking size,
    /// checksum and file signature on the way in.
    pub async fn put_local(
        &self,
        upload: &Upload<'_>,
        reader: impl AsyncRead + Unpin,
    ) -> Result<(), StorageError> {
        match &self.backend {
            Backend::Local(store) => store.put(upload, reader).await,
            Backend::S3(_) => Err(StorageError::Rejected),
        }
    }

    /// Stores bytes this server already holds, such as a picture from a form. Size,
    /// checksum and file signature are checked before anything is written, whichever
    /// storage is in use.
    pub async fn put_bytes(&self, upload: &Upload<'_>, bytes: Vec<u8>) -> Result<(), StorageError> {
        match &self.backend {
            Backend::Local(store) => store.put(upload, bytes.as_slice()).await,
            Backend::S3(store) => store
                .put_object(upload, bytes)
                .await
                .map_err(|_| StorageError::Unavailable),
        }
    }

    /// Confirms the uploaded object exists and matches what was declared.
    pub async fn verify(&self, upload: &Upload<'_>) -> Result<(), StorageError> {
        match &self.backend {
            Backend::Local(store) => store.verify(upload).await,
            Backend::S3(store) => store.verify(upload).await,
        }
    }

    /// Only bitmap images may display inline; everything else downloads.
    pub async fn download(
        &self,
        key: &str,
        filename: &str,
        inline_image: Option<&str>,
    ) -> Result<Download, StorageError> {
        match &self.backend {
            Backend::Local(store) => {
                let (file, bytes) = store.open_file(key).await?;
                Ok(Download::Stream { file, bytes })
            }
            Backend::S3(store) => Ok(Download::Redirect(
                store
                    .presigned_get(key, filename, 300, inline_image)
                    .await
                    .map_err(|_| StorageError::Unavailable)?,
            )),
        }
    }

    /// Deletes an object without waiting for a slot: when every storage slot is busy
    /// the delete is refused, like a download, and the persisted cleanup retries it.
    pub async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let _io = self.try_acquire_io()?;
        match &self.backend {
            Backend::Local(store) => store.delete(key).await,
            Backend::S3(store) => store
                .delete(key)
                .await
                .map_err(|_| StorageError::Unavailable),
        }
    }

    /// Removes staged upload files that a crash left behind. Only local storage stages
    /// files on this server.
    pub async fn remove_stale_staging(&self) -> std::io::Result<usize> {
        match &self.backend {
            Backend::Local(store) => store.remove_stale_staging().await,
            Backend::S3(_) => Ok(0),
        }
    }
}

pub(crate) fn attachment_disposition(filename: &str) -> String {
    // A declared name is not a filesystem path or a raw HTTP header value.
    let name = filename
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty() && !matches!(*name, "." | ".."))
        .unwrap_or("download");
    let name: String = name
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("attachment; filename*=UTF-8''{encoded}")
}

/// Whether the first bytes match the declared image/PDF type, so a page can't
/// be uploaded as an "image" and then served inline.
pub fn media_signature_matches(content_type: &str, bytes: &[u8]) -> bool {
    match content_type {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => {
            bytes.starts_with(b"RIFF") && bytes.get(8..12).is_some_and(|v| v == b"WEBP")
        }
        "application/pdf" => bytes.starts_with(b"%PDF-"),
        _ => true,
    }
}

/// Object keys are generated by the server (`uploads/<uuid>`); refuse anything else.
pub(crate) fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 200
        && !key.starts_with('/')
        && !key
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_type_needs_a_matching_signature() {
        assert!(!media_signature_matches(
            "image/png",
            b"<script>bad</script>"
        ));
        assert!(!media_signature_matches("image/webp", b"RIFF"));
        assert!(media_signature_matches("image/png", b"\x89PNG\r\n\x1a\n"));
        assert!(media_signature_matches("text/plain", b"anything"));
    }

    #[test]
    fn only_server_shaped_keys_are_valid() {
        assert!(valid_key("uploads/0b8f2c5e-1d4a-4f7e-9a6b-3c2d1e0f9a8b"));
        for bad in [
            "",
            "/etc/passwd",
            "uploads/../x",
            "uploads//x",
            "a b",
            "uploads/x.y",
        ] {
            assert!(!valid_key(bad), "{bad}");
        }
    }
}
