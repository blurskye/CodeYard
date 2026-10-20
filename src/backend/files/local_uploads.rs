use super::uploads::upload;
use crate::backend::{
    db::{files as files_db, DbPool},
    storage::{ObjectStore, StorageError},
};
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
    actor_id: i64,
    file_id: Uuid,
    content_type: &str,
    reader: impl AsyncRead + Unpin,
) -> Result<(), LocalUploadError> {
    let first = files_db::get_file_by_id(pool, file_id)
        .await
        .map_err(|_| LocalUploadError::MetadataUnavailable)?
        .filter(|file| file.user_id == Some(actor_id))
        .ok_or(LocalUploadError::NotFound)?;
    let time_left = first
        .upload_expires_at
        .and_then(|expires| (expires - chrono::Utc::now()).to_std().ok())
        .ok_or(LocalUploadError::NotPending)?;
    let key = first.object_key.clone();
    tokio::time::timeout(time_left, async {
        let _io = store
            .try_acquire_io()
            .map_err(|_| LocalUploadError::StorageUnavailable)?;
        let _file_lock = store
            .lock_file(&key)
            .await
            .map_err(|_| LocalUploadError::StorageUnavailable)?;
        let file = files_db::get_file_by_id(pool, file_id)
            .await
            .map_err(|_| LocalUploadError::MetadataUnavailable)?
            .filter(|file| file.user_id == Some(actor_id))
            .ok_or(LocalUploadError::NotFound)?;
        if file.is_active
            || file
                .upload_expires_at
                .is_none_or(|expires| expires <= chrono::Utc::now())
        {
            return Err(LocalUploadError::NotPending);
        }
        if file.content_type != content_type {
            return Err(LocalUploadError::Invalid);
        }
        store
            .put_local(&upload(&file), reader)
            .await
            .map_err(|error| match error {
                StorageError::Unavailable => LocalUploadError::StorageUnavailable,
                StorageError::Invalid | StorageError::NotFound | StorageError::Rejected => {
                    LocalUploadError::Rejected
                }
            })
    })
    .await
    .unwrap_or(Err(LocalUploadError::Rejected))
}
