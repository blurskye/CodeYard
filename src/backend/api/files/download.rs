use crate::backend::{
    api::session::get_current_user,
    app::AppState,
    files::{
        self, AssetDownload, DownloadCapability, DownloadError, DownloadTransport, SignedDownload,
    },
    storage::attachment_disposition,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use serde::Serialize;
use std::{io::Read, sync::Arc, time::Duration};
use uuid::Uuid;

use super::{failure, Failure};

#[derive(Serialize)]
pub struct DownloadUrlResponse {
    pub download_url: String,
    pub filename: String,
    pub content_type: String,
    pub file_size: i64,
    pub expires_in: u64,
    pub capability: DownloadCapability,
}

fn private_response(value: impl IntoResponse) -> Response {
    let mut response = value.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store, private".parse().unwrap());
    response
        .headers_mut()
        .insert(header::VARY, "Cookie, Authorization".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}

pub(super) fn parse_file_id(id: &str) -> Result<Uuid, Failure> {
    Uuid::parse_str(id).map_err(|_| {
        failure(
            StatusCode::BAD_REQUEST,
            "invalid_file_id",
            "Invalid file ID",
        )
    })
}

pub(super) fn download_response(download: SignedDownload) -> Response {
    private_response(Json(DownloadUrlResponse {
        download_url: download.url,
        filename: download.filename,
        content_type: download.content_type,
        file_size: download.file_size,
        expires_in: download.expires_in,
        capability: download.capability,
    }))
}

pub async fn get_download_url(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
) -> Result<Response, Failure> {
    let user = get_current_user(&state.pool, &headers).await?;
    let file_id = parse_file_id(&file_id)?;
    let download = files::download_for_user(&state.pool, &state.storage, user.id, file_id)
        .await
        .map_err(download_failure)?;
    Ok(download_response(download))
}

pub(super) fn asset_response(download: AssetDownload) -> Result<Response, Failure> {
    match download.transport {
        DownloadTransport::RemoteUrl(url) => Ok(private_response(Redirect::temporary(&url))),
        DownloadTransport::ApplicationStream {
            reader,
            bytes,
            io_guard,
        } => {
            let reader = reader.try_into_std().map_err(|_| {
                failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "storage_unavailable",
                    "Object reader unavailable",
                )
            })?;
            let stream = futures_util::stream::try_unfold(
                (reader, bytes, Arc::new(io_guard)),
                |(reader, remaining, io_guard)| async move {
                    if remaining == 0 {
                        return Ok::<_, std::io::Error>(None);
                    }
                    let work = download_read(reader, remaining, io_guard);
                    let DownloadRead {
                        reader,
                        buffer,
                        io_guard,
                    } = tokio::time::timeout(Duration::from_secs(30), work)
                        .await
                        .map_err(|_| {
                            std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "Object read timed out",
                            )
                        })?
                        .map_err(|_| std::io::Error::other("Object read task unavailable"))??;
                    if buffer.is_empty() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "Object stream truncated",
                        ));
                    }
                    let read = buffer.len() as i64;
                    Ok(Some((
                        bytes::Bytes::from(buffer),
                        (reader, remaining - read, io_guard),
                    )))
                },
            );
            let inline = download.inline_type.as_deref();
            let response = Response::builder()
                .header(
                    header::CONTENT_TYPE,
                    inline.unwrap_or("application/octet-stream"),
                )
                .header(
                    header::CONTENT_DISPOSITION,
                    if inline.is_some() {
                        "inline".into()
                    } else {
                        attachment_disposition(&download.filename)
                    },
                )
                .header(header::CONTENT_LENGTH, bytes.to_string())
                .header(
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; sandbox",
                )
                .body(crate::backend::api::middleware::download_body(stream))
                .map_err(|_| {
                    failure(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "response_unavailable",
                        "Object response unavailable",
                    )
                })?;
            Ok(private_response(response))
        }
    }
}

struct DownloadRead {
    reader: std::fs::File,
    buffer: Vec<u8>,
    io_guard: Arc<tokio::sync::OwnedSemaphorePermit>,
}

/// Reads the next chunk on a blocking thread; the I/O slot stays held until
/// the read returns even if the client goes away.
fn download_read(
    mut reader: std::fs::File,
    remaining: i64,
    io_guard: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> tokio::task::JoinHandle<std::io::Result<DownloadRead>> {
    tokio::task::spawn_blocking(move || {
        let mut buffer = vec![0u8; (remaining as usize).min(64 * 1024)];
        let read = reader.read(&mut buffer)?;
        buffer.truncate(read);
        Ok(DownloadRead {
            reader,
            buffer,
            io_guard,
        })
    })
}

pub(super) fn download_failure(error: DownloadError) -> Failure {
    let (status, code, message) = match error {
        DownloadError::MetadataUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "File metadata is unavailable",
        ),
        DownloadError::NotFound => (
            StatusCode::NOT_FOUND,
            "not_found",
            "File not found or not accessible",
        ),
        DownloadError::StorageUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "s3_unavailable",
            "Object storage is unavailable",
        ),
        DownloadError::SigningUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "signing_unavailable",
            "Download signing is unavailable",
        ),
        DownloadError::AccountingUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "accounting_unavailable",
            "Download accounting is unavailable",
        ),
    };
    failure(status, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropped_read_keeps_its_io_slot_until_the_read_returns() {
        use std::os::fd::FromRawFd;
        let mut descriptors = [-1; 2];
        // A pipe gives a read that blocks until the writer closes.
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
        let writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
        let capacity = Arc::new(tokio::sync::Semaphore::new(1));
        let io_guard = Arc::new(capacity.clone().acquire_owned().await.unwrap());
        drop(download_read(reader, 1, io_guard)); // the client went away
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(capacity.available_permits(), 0);
        drop(writer);
        let _released = tokio::time::timeout(Duration::from_secs(3), capacity.acquire())
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn attachment_names_never_become_paths_or_raw_headers() {
        assert_eq!(
            attachment_disposition("../../owned.txt"),
            "attachment; filename*=UTF-8''owned.txt"
        );
        assert_eq!(
            attachment_disposition(".."),
            "attachment; filename*=UTF-8''download"
        );
        let header = attachment_disposition("bad\r\n\"é.txt");
        assert!(!header.contains('\r') && !header.contains('\n'));
        assert!(axum::http::HeaderValue::from_str(&header).is_ok());
        assert!(header.contains("%22%C3%A9.txt"));
    }

    #[test]
    fn signed_capabilities_and_redirects_keep_private_security_headers() {
        for response in [
            download_response(SignedDownload {
                url: "https://objects.example.test/signed".into(),
                filename: "owned.txt".into(),
                content_type: "text/plain".into(),
                file_size: 7,
                expires_in: 300,
                capability: DownloadCapability::S3PresignedGet,
            }),
            private_response(Redirect::temporary("https://objects.example.test/signed")),
        ] {
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "no-store, private"
            );
            assert_eq!(response.headers()[header::VARY], "Cookie, Authorization");
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
        }
    }

    #[test]
    fn invalid_id_and_operation_errors_keep_wire_status_codes_and_messages() {
        let (status, Json(error)) = parse_file_id("not-a-uuid").unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error.error, "invalid_file_id");
        assert_eq!(error.message, "Invalid file ID");
        let cases = [
            (
                DownloadError::MetadataUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "database_unavailable",
                "File metadata is unavailable",
            ),
            (
                DownloadError::NotFound,
                StatusCode::NOT_FOUND,
                "not_found",
                "File not found or not accessible",
            ),
            (
                DownloadError::StorageUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "s3_unavailable",
                "Object storage is unavailable",
            ),
            (
                DownloadError::SigningUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "signing_unavailable",
                "Download signing is unavailable",
            ),
            (
                DownloadError::AccountingUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "accounting_unavailable",
                "Download accounting is unavailable",
            ),
        ];
        for (outcome, expected_status, code, message) in cases {
            let (status, Json(error)) = download_failure(outcome);
            assert_eq!(status, expected_status);
            assert_eq!(error.error, code);
            assert_eq!(error.message, message);
        }
    }
}
