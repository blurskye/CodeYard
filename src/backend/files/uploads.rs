use crate::backend::{
    db::{
        files as files_db,
        models::{File, FileReason, FileVisibility},
        DbPool,
    },
    storage::{ObjectStore, StorageError, Upload},
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;
use uuid::Uuid;

pub struct UploadDeclaration<'a> {
    pub filename: &'a str,
    pub content_type: &'a str,
    pub file_size: i64,
    pub reason: FileReason,
    pub visibility: FileVisibility,
    pub sha256_hex: &'a str,
}

/// Where the client sends the bytes: straight to S3, or to this server.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UploadCapability {
    S3DirectPut,
    AuthenticatedLocalPut,
}

#[derive(Debug)]
pub struct UploadReservation {
    pub file_id: Uuid,
    pub object_key: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub expires_in: u64,
    pub capability: UploadCapability,
    pub method: &'static str,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum UploadError {
    #[error("Invalid upload declaration")]
    Invalid,
    #[error("Upload quota exhausted")]
    Quota,
    #[error("Upload admission unavailable")]
    AdmissionUnavailable,
    #[error("Upload reserved but signing unavailable")]
    SigningUnavailable { file_id: Uuid },
    #[error("Upload storage unavailable")]
    StorageUnavailable,
    #[error("Upload failed and file cleanup is incomplete")]
    CleanupFailed { file_id: Uuid },
}

pub(super) fn upload(file: &File) -> Upload<'_> {
    Upload {
        key: &file.object_key,
        bytes: file.file_size,
        content_type: &file.content_type,
        sha256_hex: &file.sha256_hex,
    }
}

/// Records a pending file and charges the uploader's quota.
async fn reserve(
    pool: &DbPool,
    actor_id: i64,
    input: &UploadDeclaration<'_>,
) -> Result<File, UploadError> {
    files_db::reserve_upload(
        pool,
        files_db::ReserveUpload {
            user_id: actor_id,
            filename: input.filename,
            content_type: input.content_type,
            file_size: input.file_size,
            reason: input.reason,
            visibility: input.visibility,
            sha256_hex: input.sha256_hex,
        },
    )
    .await
    .map_err(|error| match error {
        files_db::AdmissionError::Invalid => UploadError::Invalid,
        files_db::AdmissionError::Quota => UploadError::Quota,
        files_db::AdmissionError::PolicyUnavailable | files_db::AdmissionError::Database(_) => {
            UploadError::AdmissionUnavailable
        }
    })
}

/// Records a pending file (charging the uploader's quota) and returns where
/// to send the bytes.
pub async fn reserve_and_sign_upload(
    pool: &DbPool,
    store: &ObjectStore,
    actor_id: i64,
    input: UploadDeclaration<'_>,
) -> Result<UploadReservation, UploadError> {
    let file = reserve(pool, actor_id, &input).await?;
    let seconds = files_db::PUT_LIFETIME_SECONDS as u64;
    let target = store
        .upload_target(
            &upload(&file),
            format!("/api/files/{}/content", file.id),
            Duration::from_secs(seconds),
        )
        .await
        .map_err(|_| UploadError::SigningUnavailable { file_id: file.id })?;
    if target.direct
        && !files_db::extend_direct_upload(pool, file.id, actor_id, files_db::PUT_LIFETIME_SECONDS)
            .await
            .map_err(|_| UploadError::SigningUnavailable { file_id: file.id })?
    {
        return Err(UploadError::SigningUnavailable { file_id: file.id });
    }
    Ok(UploadReservation {
        file_id: file.id,
        object_key: file.object_key,
        url: target.url,
        headers: target.headers,
        expires_in: seconds,
        capability: if target.direct {
            UploadCapability::S3DirectPut
        } else {
            UploadCapability::AuthenticatedLocalPut
        },
        method: "PUT",
    })
}

/// Reserve, upload and verify bytes the server already holds (a picture from a
/// form). The file stays pending until picture assignment activates it, so an
/// interrupted installation is still covered by upload expiry.
pub async fn store_upload(
    pool: &DbPool,
    store: &ObjectStore,
    actor_id: i64,
    input: UploadDeclaration<'_>,
    bytes: Vec<u8>,
) -> Result<Uuid, UploadError> {
    // Taken before the reservation, so a busy node refuses without charging the quota.
    let io = store
        .try_acquire_io()
        .map_err(|_| UploadError::StorageUnavailable)?;
    let file = reserve(pool, actor_id, &input).await?;
    let stored = async {
        let _file_lock = store
            .lock_file(&file.object_key)
            .await
            .map_err(|_| UploadError::StorageUnavailable)?;
        store
            .put_bytes(&upload(&file), bytes)
            .await
            .map_err(|_| UploadError::StorageUnavailable)?;
        verify_upload(store, &file)
            .await
            .map_err(|_| UploadError::StorageUnavailable)?;
        if !files_db::verify_pending_upload(pool, file.id, actor_id)
            .await
            .map_err(|_| UploadError::StorageUnavailable)?
        {
            return Err(UploadError::StorageUnavailable);
        }
        Ok(())
    }
    .await;
    drop(io);
    if let Err(error) = stored {
        if let Err(cleanup) = super::delete_owned_file(pool, store, actor_id, file.id).await {
            tracing::warn!(file = %file.id, %cleanup, "could not remove a failed upload");
            return Err(UploadError::CleanupFailed { file_id: file.id });
        }
        return Err(error);
    }
    Ok(file.id)
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfirmationError {
    #[error("Upload metadata unavailable")]
    MetadataUnavailable,
    #[error("Upload not found")]
    NotFound,
    #[error("Upload is expired or already confirmed")]
    NotPending,
    #[error("Upload storage is unavailable; retry")]
    StorageUnavailable,
    #[error("The uploaded file does not match its declaration")]
    VerificationFailed,
}

/// Checks that the bytes arrived as declared, then marks the file active.
pub async fn confirm_upload(
    pool: &DbPool,
    store: &ObjectStore,
    actor_id: i64,
    file_id: Uuid,
) -> Result<(), ConfirmationError> {
    let first = files_db::get_file_by_id(pool, file_id)
        .await
        .map_err(|_| ConfirmationError::MetadataUnavailable)?
        .filter(|file| file.user_id == Some(actor_id))
        .ok_or(ConfirmationError::NotFound)?;
    let _file_lock = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        store.lock_file(&first.object_key),
    )
    .await
    .map_err(|_| ConfirmationError::StorageUnavailable)?
    .map_err(|_| ConfirmationError::StorageUnavailable)?;
    let file = files_db::get_file_by_id(pool, file_id)
        .await
        .map_err(|_| ConfirmationError::MetadataUnavailable)?
        .filter(|file| file.user_id == Some(actor_id))
        .ok_or(ConfirmationError::NotFound)?;
    if file.is_active
        || file
            .upload_expires_at
            .is_none_or(|expires| expires <= chrono::Utc::now())
    {
        return Err(ConfirmationError::NotPending);
    }
    let _io = store
        .try_acquire_io()
        .map_err(|_| ConfirmationError::StorageUnavailable)?;
    verify_upload(store, &file).await?;
    if !files_db::activate_upload(pool, file_id, actor_id)
        .await
        .map_err(|_| ConfirmationError::MetadataUnavailable)?
    {
        return Err(ConfirmationError::NotPending);
    }
    Ok(())
}

async fn verify_upload(store: &ObjectStore, file: &File) -> Result<(), ConfirmationError> {
    store
        .verify(&upload(file))
        .await
        .map_err(|error| match error {
            StorageError::Unavailable => ConfirmationError::StorageUnavailable,
            StorageError::Invalid | StorageError::NotFound | StorageError::Rejected => {
                ConfirmationError::VerificationFailed
            }
        })
}
