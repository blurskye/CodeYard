use crate::backend::storage::ObjectStore;
use codeyard_db_core::DbPool;
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

/// Removes the owner's metadata immediately and queues the bytes in the same
/// transaction. The sweep removes them after the restore window.
pub async fn delete_owned_file(
    pool: &DbPool,
    store: &ObjectStore,
    actor: impl Into<crate::backend::db::files::FileAuthority>,
    file_id: Uuid,
) -> Result<(), DeleteError> {
    let actor = actor.into();
    let actor_id = actor.user_id.ok_or(DeleteError::Denied)?;
    let first = crate::backend::db::files::get_owned_file(pool, file_id, actor_id)
        .await
        .map_err(|_| DeleteError::MetadataUnavailable)?
        .ok_or(DeleteError::NotFound)?;
    let _file_lock = store
        .lock_file(&first.object_key)
        .await
        .map_err(|_| DeleteError::StorageUnavailable)?;
    match crate::backend::db::files::delete_file(pool, file_id, actor)
        .await
        .map_err(|_| DeleteError::MetadataUnavailable)?
    {
        crate::backend::db::files::DeleteFileOutcome::Deleted(_) => Ok(()),
        crate::backend::db::files::DeleteFileOutcome::NotFound => Err(DeleteError::NotFound),
        crate::backend::db::files::DeleteFileOutcome::Denied => Err(DeleteError::Denied),
        crate::backend::db::files::DeleteFileOutcome::ReferencedOrganizationAvatar => {
            Err(DeleteError::ReferencedOrganizationAvatar)
        }
        crate::backend::db::files::DeleteFileOutcome::ReferencedReleaseAsset => {
            Err(DeleteError::ReferencedReleaseAsset)
        }
    }
}

/// Removes an unused picture only if no current avatar pointer references it. A
/// picture an account replaces is removed by that account's own transaction.
pub async fn delete_unreferenced_file(
    pool: &DbPool,
    store: &ObjectStore,
    actor_id: i64,
    file_id: Uuid,
) -> Result<bool, DeleteError> {
    let Some(first) = crate::backend::db::files::get_owned_file(pool, file_id, actor_id)
        .await
        .map_err(|_| DeleteError::MetadataUnavailable)?
    else {
        return Ok(false);
    };
    let _file_lock = store
        .lock_file(&first.object_key)
        .await
        .map_err(|_| DeleteError::StorageUnavailable)?;
    Ok(matches!(
        crate::backend::db::files::delete_unreferenced_file(pool, file_id, actor_id)
            .await
            .map_err(|_| DeleteError::MetadataUnavailable)?,
        crate::backend::db::files::DeleteUnreferencedOutcome::Deleted(_)
    ))
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
    let expired = crate::backend::db::files::expired_uploads(pool, EXPIRED_LIMIT).await?;
    for candidate in expired {
        let expire = async {
            let _file_lock = store.lock_file(&candidate.object_key).await?;
            Ok::<_, anyhow::Error>(
                crate::backend::db::files::delete_expired_upload(pool, candidate.id).await?,
            )
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

    let due = crate::backend::db::files::due_object_deletions(pool, OBJECT_LIMIT).await?;
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
            crate::backend::db::files::finish_object_deletion(pool, &key)
                .await
                .map(|_| ())
        } else if deleted && due {
            // S3 expiry limits when a PUT starts, not when it finishes. Keep its tombstone
            // and reconcile late objects even after a successful DELETE.
            crate::backend::db::files::postpone_object_deletion(pool, &key, 24 * 60 * 60).await
        } else if due {
            crate::backend::db::files::postpone_object_deletion(pool, &key, OBJECT_RETRY_SECONDS)
                .await
        } else {
            Ok(())
        };
        recorded?;
    }
    // Work cancelled at the batch deadline must not occupy the next batch again.
    for key in remaining {
        crate::backend::db::files::postpone_object_deletion(pool, &key, OBJECT_RETRY_SECONDS)
            .await?;
    }
    Ok(removed)
}
