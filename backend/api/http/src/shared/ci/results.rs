//! The Results Twirp JSON API and Azure block-blob subset used by Actions v4.
use axum::body::Bytes;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_app::storage::StorageError;
use codeyard_db_actions::action_objects::ActionObject;
use codeyard_db_actions::action_objects::BlobPart;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
pub struct Error(pub StatusCode, pub &'static str, pub &'static str);

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"code":self.1,"msg":self.2}))).into_response()
    }
}

impl From<anyhow::Error> for Error {
    fn from(_: anyhow::Error) -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Results storage unavailable",
        )
    }
}

impl From<StorageError> for Error {
    fn from(_: StorageError) -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Object storage unavailable",
        )
    }
}

pub fn invalid() -> Error {
    Error(
        StatusCode::BAD_REQUEST,
        "invalid_argument",
        "Invalid Results request",
    )
}

pub fn missing() -> Error {
    Error(StatusCode::NOT_FOUND, "not_found", "Entry not found")
}

pub fn size(value: &Value) -> Result<i64, Error> {
    let size = value
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| value.as_i64())
        .ok_or_else(invalid)?;
    if size < 0 {
        return Err(invalid());
    }
    Ok(size)
}

pub async fn download(
    state: Arc<AppState>,
    object: ActionObject,
    headers: &HeaderMap,
) -> Result<Response, Error> {
    let permit = state.storage.try_acquire_download().ok_or(Error(
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "Downloads busy",
    ))?;
    let parts: Vec<BlobPart> =
        serde_json::from_str(object.manifest.as_deref().ok_or_else(missing)?)
            .map_err(|_| invalid())?;
    let invalid_range = || {
        Error(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "out_of_range",
            "Invalid byte range",
        )
    };
    let name = if headers.contains_key(header::RANGE) {
        "range"
    } else {
        "x-ms-range"
    };
    let (range, partial) =
        crate::range::requested_header(headers, name).map_err(|_| invalid_range())?;
    let size = u64::try_from(object.size).map_err(|_| invalid_range())?;
    let (start, length) = range.normalize(size).map_err(|_| invalid_range())?;
    let (start, end) = (start as i64, (start + length) as i64);
    let store = state.storage.clone();
    let stream = futures_util::stream::try_unfold(
        (
            parts,
            start,
            end,
            0usize,
            0i64,
            None::<Box<dyn tokio::io::AsyncRead + Unpin + Send>>,
            store,
            permit,
        ),
        |(parts, offset, end, mut index, mut base, mut reader, store, permit)| async move {
            if offset >= end {
                return Ok::<_, std::io::Error>(None);
            }
            while let Some(part) = parts.get(index) {
                if offset < base + part.size {
                    break;
                }
                base += part.size;
                index += 1;
                reader = None;
            }
            let part = parts
                .get(index)
                .ok_or_else(|| std::io::Error::other("Missing blob block"))?;
            let remaining = (end - offset).min(base + part.size - offset);
            if reader.is_none() {
                reader = Some(
                    tokio::time::timeout(
                        Duration::from_secs(60),
                        store.open_range(
                            &part.object_key,
                            (offset - base) as u64,
                            remaining as u64,
                        ),
                    )
                    .await
                    .map_err(|_| std::io::Error::other("Blob read timeout"))?
                    .map_err(|_| std::io::Error::other("Blob read unavailable"))?,
                );
            }
            let mut bytes = vec![0; remaining.min(1024 * 1024) as usize];
            use tokio::io::AsyncReadExt;
            let length = tokio::time::timeout(
                Duration::from_secs(60),
                reader
                    .as_mut()
                    .expect("range reader exists")
                    .read(&mut bytes),
            )
            .await
            .map_err(|_| std::io::Error::other("Blob read timeout"))??;
            if length == 0 {
                return Err(std::io::Error::other("Short blob block"));
            }
            bytes.truncate(length);
            Ok(Some((
                Bytes::from(bytes),
                (
                    parts,
                    offset + length as i64,
                    end,
                    index,
                    base,
                    reader,
                    store,
                    permit,
                ),
            )))
        },
    );
    let mut response = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(
            header::CONTENT_TYPE,
            if object.kind == "artifact" {
                "application/zip"
            } else {
                "application/octet-stream"
            },
        )
        .header(header::CONTENT_LENGTH, (end - start).to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, format!("\"{}\"", object.id))
        .header(header::CONTENT_ENCODING, "identity")
        .header(header::CACHE_CONTROL, "private, no-store, no-transform")
        .header(
            header::CONTENT_DISPOSITION,
            codeyard_app::storage::attachment_disposition(&format!("{}.zip", object.name)),
        );
    if partial {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{}", end - 1, object.size),
        );
    }
    // The send deadline frees the download slot from a reader that stops reading.
    Ok(response
        .body(crate::middleware::download_body(stream))
        .expect("static blob headers"))
}
