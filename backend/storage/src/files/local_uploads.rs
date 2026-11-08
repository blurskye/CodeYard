use crate::files::uploads::upload;
use crate::ObjectStore;
use crate::StorageError;
use codeyard_database::DbPool;
use tokio::io::AsyncRead;
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LocalUploadError {
    #[error("Upload metadata unavailable")]
    MetadataUnavailable,
    #[error("Upload not found")]
    NotFound,
    #[error("Upload is expired or already confirmed")]
    NotPending,
    #[error("Upload declaration differs from the reservation")]
    Invalid,
    #[error("Object storage unavailable")]
    StorageUnavailable,
    #[error("Upload body was rejected")]
    Rejected,
}

/// Receives the bytes of the actor's own pending upload (local storage only).
/// They must arrive before the reservation expires, so a sender that stalls
/// cannot keep a storage slot busy for longer than that.
pub async fn put_local_upload(
    pool: &DbPool,
    store: &ObjectStore,
    actor: impl Into<codeyard_db_repos::files::FileAuthority>,
    file_id: Uuid,
    content_type: &str,
    reader: impl AsyncRead + Unpin,
) -> Result<(), LocalUploadError> {
    let actor = actor.into();
    let actor_id = actor.user_id.ok_or(LocalUploadError::NotFound)?;
    let first = codeyard_db_repos::files::get_owned_file(pool, file_id, actor_id)
        .await
        .map_err(|_| LocalUploadError::MetadataUnavailable)?
        .ok_or(LocalUploadError::NotFound)?;
    let time_left = first
        .upload_expires_at
        .and_then(|expires| (expires - chrono::Utc::now()).to_std().ok())
        .ok_or(LocalUploadError::NotPending)?;
    let key = first.object_key.clone();
    tokio::time::timeout(time_left, async {
        // Wait for the file first: PUTs queued on one file must not hold the shared I/O slots.
        let _file_lock = store
            .lock_file(&key)
            .await
            .map_err(|_| LocalUploadError::StorageUnavailable)?;
        let io = std::sync::Arc::new(
            store
                .try_acquire_io()
                .map_err(|_| LocalUploadError::StorageUnavailable)?,
        );
        let file = codeyard_db_repos::files::get_owned_file(pool, file_id, actor_id)
            .await
            .map_err(|_| LocalUploadError::MetadataUnavailable)?
            .ok_or(LocalUploadError::NotFound)?;
        if !file.is_pending() {
            return Err(LocalUploadError::NotPending);
        }
        if file.content_type != content_type {
            return Err(LocalUploadError::Invalid);
        }
        let staged = store
            .stage_local(&upload(&file), reader, io.clone())
            .await
            .map_err(|error| match error {
                StorageError::Unavailable => LocalUploadError::StorageUnavailable,
                _ => LocalUploadError::Rejected,
            })?;
        if !codeyard_db_repos::files::can_publish_upload(pool, file_id, actor)
            .await
            .map_err(|_| LocalUploadError::MetadataUnavailable)?
        {
            return Err(LocalUploadError::Rejected);
        }
        staged
            .publish()
            .await
            .map_err(|_| LocalUploadError::StorageUnavailable)
    })
    .await
    .unwrap_or(Err(LocalUploadError::Rejected))
}
