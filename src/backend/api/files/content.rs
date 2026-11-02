use crate::backend::{
    api::http::session::get_current_user,
    app::AppState,
    files::{self, LocalUploadError},
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
};
use bytes::Bytes;
use futures_util::Stream;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, ReadBuf};
use uuid::Uuid;

use crate::backend::api::http::error::{ApiError, ApiFailure};

/// Receives upload bytes when files are stored locally (cookie-authenticated).
pub async fn put_upload_content(
    State(state): State<Arc<AppState>>,
    Path(file_id): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiFailure> {
    let user = get_current_user(&state.pool, &headers).await?;
    let mut types = headers.get_all(header::CONTENT_TYPE).iter();
    let content_type = types
        .next()
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| local_failure(LocalUploadError::Invalid))?;
    if types.next().is_some() {
        return Err(local_failure(LocalUploadError::Invalid));
    }
    let mut lengths = headers.get_all(header::CONTENT_LENGTH).iter();
    if let Some(length) = lengths.next() {
        let length = length
            .to_str()
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| local_failure(LocalUploadError::Invalid))?;
        if lengths.next().is_some() || !(1..=100 * 1024 * 1024).contains(&length) {
            return Err(ApiError::failure(
                StatusCode::PAYLOAD_TOO_LARGE,
                "upload_too_large",
                "Upload exceeds the bounded declaration",
            ));
        }
    }
    let reader = UploadReader {
        stream: Box::pin(body.into_data_stream()),
        chunk: Bytes::new(),
    };
    files::put_local_upload(
        &state.pool,
        &state.storage,
        user.id,
        file_id,
        content_type,
        reader,
    )
    .await
    .map_err(local_failure)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Keeps ownership of the body (including any body-scoped middleware guards)
/// through filesystem IO. Holds at most the current body frame; never collects.
struct UploadReader {
    stream: Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    chunk: Bytes,
}
impl AsyncRead for UploadReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        // Empty HTTP/2 DATA frames must not defeat a cooperative timeout by
        // keeping a single poll busy forever without consuming admitted bytes.
        for _ in 0..64 {
            if !self.chunk.is_empty() {
                let take = output.remaining().min(self.chunk.len());
                output.put_slice(&self.chunk.split_to(take));
                return Poll::Ready(Ok(()));
            }
            match self.stream.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Ok(chunk))) => self.chunk = chunk,
                Poll::Ready(Some(Err(_))) => {
                    return Poll::Ready(Err(std::io::Error::other("Upload body unavailable")))
                }
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
fn local_failure(error: LocalUploadError) -> ApiFailure {
    let (status, code) = match error {
        LocalUploadError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        LocalUploadError::NotPending => (StatusCode::CONFLICT, "not_pending"),
        LocalUploadError::Invalid => (StatusCode::BAD_REQUEST, "invalid_upload"),
        LocalUploadError::Rejected => (StatusCode::UNPROCESSABLE_ENTITY, "upload_rejected"),
        LocalUploadError::MetadataUnavailable | LocalUploadError::StorageUnavailable => {
            (StatusCode::SERVICE_UNAVAILABLE, "upload_unavailable")
        }
    };
    ApiError::failure(status, code, &error.to_string())
}
