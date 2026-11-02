use crate::backend::{
    db::{files as files_db, models::File, DbPool},
    storage::{Download, ObjectStore},
};
use serde::Serialize;
use tokio::sync::OwnedSemaphorePermit;
use uuid::Uuid;

const DOWNLOAD_LIFETIME_SECONDS: u64 = 300;
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DownloadCapability {
    S3PresignedGet,
    AuthorizedApplicationGet,
}
#[derive(Debug)]
pub struct SignedDownload {
    pub url: String,
    pub filename: String,
    pub content_type: String,
    pub file_size: i64,
    pub expires_in: u64,
    pub capability: DownloadCapability,
}
#[derive(Debug)]
pub enum DownloadTransport {
    RemoteUrl(String),
    ApplicationStream {
        reader: tokio::fs::File,
        bytes: i64,
        io_guard: OwnedSemaphorePermit,
    },
}
#[derive(Debug)]
pub struct AssetDownload {
    pub transport: DownloadTransport,
    pub inline_type: Option<String>,
    pub filename: String,
    pub file_size: i64,
}
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DownloadError {
    #[error("File metadata unavailable")]
    MetadataUnavailable,
    #[error("File not found or not accessible")]
    NotFound,
    #[error("Object storage unavailable")]
    StorageUnavailable,
    #[error("Download signing unavailable")]
    SigningUnavailable,
    #[error("Download accounting unavailable")]
    AccountingUnavailable,
}

/// Records the download, then returns where to fetch the file.
pub async fn download_for_user(
    pool: &DbPool,
    store: &ObjectStore,
    actor_id: i64,
    file_id: Uuid,
) -> Result<SignedDownload, DownloadError> {
    let file = authorize(pool, Some(actor_id), file_id).await?;
    let (url, capability) = capability(store, &file).await?;
    files_db::record_download(pool, actor_id, file.id, file.reason.as_deref(), None)
        .await
        .map_err(|_| DownloadError::AccountingUnavailable)?;
    Ok(download_result(file, url, capability))
}

/// Anonymous downloads only reach public files and avatars.
pub async fn download_asset(
    pool: &DbPool,
    store: &ObjectStore,
    actor_id: Option<i64>,
    file_id: Uuid,
) -> Result<SignedDownload, DownloadError> {
    let file = authorize(pool, actor_id, file_id).await?;
    let (url, capability) = capability(store, &file).await?;
    Ok(download_result(file, url, capability))
}

async fn authorize(
    pool: &DbPool,
    actor: Option<i64>,
    file_id: Uuid,
) -> Result<File, DownloadError> {
    files_db::get_downloadable_file(pool, file_id, actor)
        .await
        .map_err(|_| DownloadError::MetadataUnavailable)?
        .ok_or(DownloadError::NotFound)
}

/// Only avatar bitmaps display inline; everything else downloads as a file.
fn inline_type(file: &File) -> Option<&str> {
    (matches!(file.reason.as_deref(), Some("avatar" | "org_avatar"))
        && matches!(
            file.content_type.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ))
    .then_some(file.content_type.as_str())
}

async fn capability(
    store: &ObjectStore,
    file: &File,
) -> Result<(String, DownloadCapability), DownloadError> {
    if store.kind() == "local" {
        return Ok((
            format!("/api/files/{}/content", file.id),
            DownloadCapability::AuthorizedApplicationGet,
        ));
    }
    match store
        .download(&file.object_key, &file.original_name, inline_type(file))
        .await
    {
        Ok(Download::Redirect(url)) => Ok((url, DownloadCapability::S3PresignedGet)),
        _ => Err(DownloadError::SigningUnavailable),
    }
}

/// Opens a file for the HTML asset routes: a redirect to S3, or a local
/// stream that holds a download slot until the response body is done.
pub async fn open_asset_download(
    pool: &DbPool,
    store: &ObjectStore,
    actor: Option<i64>,
    file_id: Uuid,
) -> Result<AssetDownload, DownloadError> {
    let file = authorize(pool, actor, file_id).await?;
    open_file(store, file).await
}

/// Opens a file the caller has already authorized for this reader. A release asset uses this
/// after it checked the repository the release belongs to.
pub async fn open_file(store: &ObjectStore, file: File) -> Result<AssetDownload, DownloadError> {
    let inline_type = inline_type(&file).map(str::to_owned);
    let io_guard = store
        .try_acquire_download()
        .ok_or(DownloadError::StorageUnavailable)?;
    let transport = match store
        .download(
            &file.object_key,
            &file.original_name,
            inline_type.as_deref(),
        )
        .await
        .map_err(|_| DownloadError::StorageUnavailable)?
    {
        Download::Redirect(url) => DownloadTransport::RemoteUrl(url),
        Download::Stream {
            file: reader,
            bytes,
        } => DownloadTransport::ApplicationStream {
            reader,
            bytes: bytes as i64,
            io_guard,
        },
    };
    Ok(AssetDownload {
        transport,
        inline_type,
        filename: file.original_name,
        file_size: file.file_size,
    })
}

fn download_result(file: File, url: String, capability: DownloadCapability) -> SignedDownload {
    SignedDownload {
        url,
        filename: file.original_name,
        content_type: file.content_type,
        file_size: file.file_size,
        expires_in: if capability == DownloadCapability::S3PresignedGet {
            DOWNLOAD_LIFETIME_SECONDS
        } else {
            0
        },
        capability,
    }
}
