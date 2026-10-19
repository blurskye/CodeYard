use crate::backend::{
    db::{files as files_db, DbPool},
    storage::ObjectStore,
};
use uuid::Uuid;

/// A key whose bytes could not be removed is tried again after this long.
const OBJECT_RETRY_SECONDS: i64 = 900;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeleteError {
    #[error("File metadata unavailable")]
    MetadataUnavailable,
    #[error("File storage unavailable")]
    StorageUnavailable,
    #[error("File not found")]
    NotFound,
    #[error("The file is the current picture of an organization")]
    ReferencedOrganizationAvatar,
    #[error("The file is a release asset")]
    ReferencedReleaseAsset,
}

/// Removes the owner's metadata immediately and records durable object cleanup in
/// the same transaction. The immediate object delete is best-effort: the persisted
/// work remains until any PUT that could still arrive has had its grace period.
pub async fn delete_owned_file(
    pool: &DbPool,
    store: &ObjectStore,
    actor_id: i64,
    file_id: Uuid,
) -> Result<(), DeleteError> {
    let first = files_db::get_file_by_id(pool, file_id)
        .await
        .map_err(|_| DeleteError::MetadataUnavailable)?
        .filter(|file| file.user_id == Some(actor_id))
        .ok_or(DeleteError::NotFound)?;
    let _file_lock = store
        .lock_file(&first.object_key)
        .await
        .map_err(|_| DeleteError::StorageUnavailable)?;
    let file = match files_db::delete_file(pool, file_id, actor_id)
        .await
        .map_err(|_| DeleteError::MetadataUnavailable)?
    {
        files_db::DeleteFileOutcome::Deleted(file) => file,
        files_db::DeleteFileOutcome::NotFound => return Err(DeleteError::NotFound),
        files_db::DeleteFileOutcome::ReferencedOrganizationAvatar => {
            return Err(DeleteError::ReferencedOrganizationAvatar)
        }
        files_db::DeleteFileOutcome::ReferencedReleaseAsset => {
            return Err(DeleteError::ReferencedReleaseAsset)
        }
    };
    if let Err(error) = store.delete(&file.object_key).await {
        tracing::warn!(file = %file.id, %error, "uploaded object cleanup deferred");
    }
    Ok(())
}

/// Removes an unused picture only if no current avatar pointer references it. A
/// picture an account replaces is removed by that account's own transaction.
pub async fn delete_unreferenced_file(
    pool: &DbPool,
    store: &ObjectStore,
    actor_id: i64,
    file_id: Uuid,
) -> Result<bool, DeleteError> {
    let Some(first) = files_db::get_file_by_id(pool, file_id)
        .await
        .map_err(|_| DeleteError::MetadataUnavailable)?
        .filter(|file| file.user_id == Some(actor_id))
    else {
        return Ok(false);
    };
    let _file_lock = store
        .lock_file(&first.object_key)
        .await
        .map_err(|_| DeleteError::StorageUnavailable)?;
    let file = match files_db::delete_unreferenced_file(pool, file_id, actor_id)
        .await
        .map_err(|_| DeleteError::MetadataUnavailable)?
    {
        files_db::DeleteUnreferencedOutcome::Deleted(file) => file,
        files_db::DeleteUnreferencedOutcome::NotFound
        | files_db::DeleteUnreferencedOutcome::Referenced => return Ok(false),
    };
    if let Err(error) = store.delete(&file.object_key).await {
        tracing::warn!(file = %file.id, %error, "picture object cleanup deferred");
    }
    Ok(true)
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("Upload expiry metadata unavailable after removing {removed} files")]
pub struct ExpiryError {
    pub removed: usize,
    /// Set when metadata was removed but updating its cleanup work failed.
    pub object_removed: Option<Uuid>,
}

/// One bounded sweep. Expired metadata and due object work have independent
/// budgets, so either backlog keeps making progress. No database transaction
/// spans an object-store operation.
pub async fn remove_expired_uploads(
    pool: &DbPool,
    store: &ObjectStore,
) -> Result<usize, ExpiryError> {
    const EXPIRED_LIMIT: i64 = 100;
    const OBJECT_LIMIT: i64 = 100;
    use futures_util::{stream, StreamExt};
    use std::{collections::HashMap, time::Duration};
    let mut removed = 0;
    let mut objects = HashMap::new();
    let metadata_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let expired = files_db::expired_uploads(pool, EXPIRED_LIMIT)
        .await
        .map_err(|_| ExpiryError {
            removed,
            object_removed: None,
        })?;
    for candidate in expired {
        let expire = async {
            let _file_lock =
                store
                    .lock_file(&candidate.object_key)
                    .await
                    .map_err(|_| ExpiryError {
                        removed,
                        object_removed: None,
                    })?;
            files_db::delete_expired_upload(pool, candidate.id)
                .await
                .map_err(|_| ExpiryError {
                    removed,
                    object_removed: Some(candidate.id),
                })
        };
        let file = match tokio::time::timeout_at(metadata_deadline, expire).await {
            Ok(result) => result?,
            Err(_) => break,
        };
        if let Some(file) = file {
            removed += 1;
            objects.insert(file.object_key, false);
        }
    }

    let due = files_db::due_object_deletions(pool, OBJECT_LIMIT)
        .await
        .map_err(|_| ExpiryError {
            removed,
            object_removed: None,
        })?;
    for key in due {
        objects.insert(key, true);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let deletions = stream::iter(objects)
        .map(|(key, due)| async move {
            let deleted = match store.lock_file(&key).await {
                Ok(_file_lock) => store.delete(&key).await.is_ok(),
                Err(_) => false,
            };
            (key, due, deleted)
        })
        .buffer_unordered(4);
    tokio::pin!(deletions);
    while let Ok(Some((key, due, deleted))) =
        tokio::time::timeout_at(deadline, deletions.next()).await
    {
        let recorded = if deleted {
            files_db::finish_object_deletion(pool, &key)
                .await
                .map(|_| ())
        } else if due {
            files_db::postpone_object_deletion(pool, &key, OBJECT_RETRY_SECONDS).await
        } else {
            Ok(())
        };
        recorded.map_err(|_| ExpiryError {
            removed,
            object_removed: None,
        })?;
    }
    Ok(removed)
}
