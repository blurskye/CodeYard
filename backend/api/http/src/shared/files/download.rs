use crate::error::ApiError;
use crate::error::ApiFailure;
use axum::http::header;
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Redirect;
use axum::response::Response;
use codeyard_storage::attachment_disposition;
use codeyard_storage::files::downloads::AssetDownload;
use codeyard_storage::files::downloads::DownloadTransport;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
pub fn private_response(value: impl IntoResponse) -> Response {
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

pub fn asset_response(download: AssetDownload, method: Method) -> Result<Response, ApiFailure> {
    if method == Method::HEAD {
        return asset_body(
            &download.filename,
            download.inline_type.as_deref(),
            download.file_size,
            axum::body::Body::empty(),
        );
    }
    match download.transport {
        DownloadTransport::RemoteUrl(url) => Ok(private_response(Redirect::temporary(&url))),
        DownloadTransport::ApplicationStream {
            reader,
            bytes,
            io_guard,
        } => {
            let reader = reader.try_into_std().map_err(|_| {
                ApiError::failure(
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
            asset_body(
                &download.filename,
                download.inline_type.as_deref(),
                bytes,
                crate::middleware::download_body(stream),
            )
        }
    }
}

fn asset_body(
    filename: &str,
    inline: Option<&str>,
    size: i64,
    body: axum::body::Body,
) -> Result<Response, ApiFailure> {
    let response = Response::builder()
        .header(
            header::CONTENT_TYPE,
            inline.unwrap_or("application/octet-stream"),
        )
        .header(
            header::CONTENT_DISPOSITION,
            if inline.is_some() {
                "inline".to_owned()
            } else {
                attachment_disposition(filename)
            },
        )
        .header(header::CONTENT_LENGTH, size.to_string())
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        )
        .body(body)
        .map_err(|_| {
            ApiError::failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response_unavailable",
                "Object response unavailable",
            )
        })?;
    Ok(private_response(response))
}

pub struct DownloadRead {
    reader: std::fs::File,
    buffer: Vec<u8>,
    io_guard: Arc<tokio::sync::OwnedSemaphorePermit>,
}

/// Reads the next chunk on a blocking thread; the I/O slot stays held until
/// the read returns even if the client goes away.
pub fn download_read(
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
