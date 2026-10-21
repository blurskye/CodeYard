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
    #[error("File deletion is not allowed")]
    Denied,
    #[error("The file is the current picture of an organization")]
    ReferencedOrganizationAvatar,
    #[error("The file is a release asset")]
    ReferencedReleaseAsset,
}

/// Removes the owner's metadata immediately and records durable object cleanup in
/// the same transaction. The immediate object delete is best-effort: the persisted
/// work survives the initial PUT grace period; S3 keys remain queued for reconciliation.
pub async fn delete_owned_file(
    pool: &DbPool,
    store: &ObjectStore,
    actor_id: i64,
    file_id: Uuid,
) -> Result<(), DeleteError> {
    let first = files_db::get_owned_file(pool, file_id, actor_id)
        .await
        .map_err(|_| DeleteError::MetadataUnavailable)?
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
        files_db::DeleteFileOutcome::Denied => return Err(DeleteError::Denied),
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
    let Some(first) = files_db::get_owned_file(pool, file_id, actor_id)
        .await
        .map_err(|_| DeleteError::MetadataUnavailable)?
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

/// One bounded sweep. Expired metadata and due object work have independent
/// budgets, so either backlog keeps making progress. No database transaction
/// spans an object-store operation.
pub async fn remove_expired_uploads(pool: &DbPool, store: &ObjectStore) -> anyhow::Result<usize> {
    const EXPIRED_LIMIT: i64 = 100;
    const OBJECT_LIMIT: i64 = 100;
    use futures_util::{stream, StreamExt};
    use std::{collections::HashMap, time::Duration};
    let mut removed = 0;
    let mut objects = HashMap::new();
    let metadata_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let expired = files_db::expired_uploads(pool, EXPIRED_LIMIT).await?;
    for candidate in expired {
        let expire = async {
            let _file_lock = store.lock_file(&candidate.object_key).await?;
            Ok::<_, anyhow::Error>(files_db::delete_expired_upload(pool, candidate.id).await?)
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

    let due = files_db::due_object_deletions(pool, OBJECT_LIMIT).await?;
    for key in due {
        objects.insert(key, true);
    }
    let mut remaining: std::collections::HashSet<_> = objects
        .iter()
        .filter(|(_, due)| **due)
        .map(|(key, _)| key.clone())
        .collect();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let deletions = stream::iter(objects)
        .map(|(key, due)| async move {
            let deleted = tokio::time::timeout(Duration::from_secs(1), async {
                let _file_lock = store.lock_file(&key).await?;
                store.delete(&key).await
            })
            .await
            .is_ok_and(|result| result.is_ok());
            (key, due, deleted)
        })
        .buffer_unordered(4);
    tokio::pin!(deletions);
    while let Ok(Some((key, due, deleted))) =
        tokio::time::timeout_at(deadline, deletions.next()).await
    {
        remaining.remove(&key);
        let recorded = if deleted && store.kind() == "local" {
            files_db::finish_object_deletion(pool, &key)
                .await
                .map(|_| ())
        } else if deleted && due {
            // S3 expiry limits when a PUT starts, not when it finishes. Keep its tombstone
            // and reconcile late objects even after a successful DELETE.
            files_db::postpone_object_deletion(pool, &key, 24 * 60 * 60).await
        } else if due {
            files_db::postpone_object_deletion(pool, &key, OBJECT_RETRY_SECONDS).await
        } else {
            Ok(())
        };
        recorded?;
    }
    // Work cancelled at the batch deadline must not occupy the next batch again.
    for key in remaining {
        files_db::postpone_object_deletion(pool, &key, OBJECT_RETRY_SECONDS).await?;
    }
    Ok(removed)
}
