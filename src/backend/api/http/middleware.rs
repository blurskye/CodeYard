//! Node access and private representation middleware.
use super::{
    error::ApiError,
    session::{self, get_current_user},
};
use crate::backend::api::{
    files,
    repos::{code::upload, releases},
};
use crate::backend::{app::AppState, db};
use axum::{
    extract::{MatchedPath, Request},
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json,
};
use futures_util::{Stream, StreamExt};
use std::{io, sync::Arc, time::Duration};

/// The upload routes are matched before this layer runs. A request without a matched path is
/// not an upload route, so it is gated. Upload routes read their own bodies, with their own
/// limits, and answer a too-large file with their own response.
fn streams_upload(request: &Request) -> bool {
    request
        .extensions()
        .get::<MatchedPath>()
        .is_some_and(|path| {
            (*request.method() == Method::PUT && path.as_str() == files::CONTENT_ROUTE)
                || (*request.method() == Method::POST
                    && matches!(path.as_str(), upload::ROUTE | releases::ASSET_ROUTE))
        })
}

/// Reads the whole body of every POST, PUT, PATCH and DELETE request before its handler
/// runs. At most 8 reads run at once per node, each within 4 MiB and 30 seconds. A slot
/// covers only the read, so a slow handler does not hold one. The upload routes are exempt.
pub(crate) async fn bounded_form_body(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if !matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) || streams_upload(&request)
    {
        return next.run(request).await;
    }
    let Ok(permit) = state.form_reads.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(
        Duration::from_secs(30),
        axum::body::to_bytes(body, 4 * 1024 * 1024),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    drop(permit);
    next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
        .await
}

struct DownloadTask(tokio::task::JoinHandle<io::Result<()>>);

impl Drop for DownloadTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The producer owns the allocation or file permit. Its send deadline runs even
/// when Hyper stops polling the response. Only one small chunk remains queued.
pub(crate) fn download_body(
    stream: impl Stream<Item = io::Result<bytes::Bytes>> + Send + 'static,
) -> axum::body::Body {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let task = DownloadTask(tokio::spawn(async move {
        let mut stream = Box::pin(stream);
        loop {
            let chunk = tokio::select! {
                _ = sender.closed() => return Ok(()),
                chunk = stream.next() => match chunk {
                    Some(chunk) => chunk?,
                    None => return Ok(()),
                },
            };
            match tokio::time::timeout(Duration::from_secs(30), sender.send(chunk)).await {
                Ok(Ok(())) => (),
                Ok(Err(_)) => return Ok(()),
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Download timed out",
                    ))
                }
            }
        }
    }));
    axum::body::Body::from_stream(futures_util::stream::try_unfold(
        (receiver, task),
        |(mut receiver, mut task)| async move {
            match receiver.recv().await {
                Some(chunk) => Ok(Some((chunk, (receiver, task)))),
                None => {
                    (&mut task.0)
                        .await
                        .map_err(|_| io::Error::other("Download task unavailable"))??;
                    Ok::<_, io::Error>(None)
                }
            }
        },
    ))
}

pub(crate) fn buffered_download_body(
    bytes: Vec<u8>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> axum::body::Body {
    download_body(futures_util::stream::try_unfold(
        (bytes, 0, permit),
        |(bytes, offset, permit)| async move {
            if offset == bytes.len() {
                return Ok::<_, io::Error>(None);
            }
            let end = (offset + 64 * 1024).min(bytes.len());
            // A queued chunk must not keep the whole allocation alive after timeout.
            let chunk = bytes::Bytes::copy_from_slice(&bytes[offset..end]);
            Ok(Some((chunk, (bytes, end, permit))))
        },
    ))
}

pub(crate) async fn require_node_access(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let node_config = match db::node_config::get_node_configuration(&state.pool).await {
        Ok(config) => config,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Failed to load node configuration",
                )),
            )
                .into_response();
        }
    };

    let (setup_completed, private_mode) = match node_config {
        Some(config) => (
            config.setup_completed,
            config.node_mode.eq_ignore_ascii_case("private"),
        ),
        None => (false, false),
    };

    if !setup_completed {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Resource not found")),
        )
            .into_response();
    }

    if !private_mode {
        return next.run(request).await;
    }

    match get_current_user(&state.pool, request.headers()).await {
        Ok(_) => next.run(request).await,
        // A session-store outage is not evidence that the caller is anonymous.
        Err((status, body)) if status.is_server_error() => (status, body).into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Resource not found")),
        )
            .into_response(),
    }
}

pub(crate) async fn private_sensitive_response(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store, private".parse().expect("static cache policy"),
    );
    if !response
        .headers()
        .get_all(header::VARY)
        .iter()
        .any(|value| value == "Cookie, Authorization")
    {
        response.headers_mut().append(
            header::VARY,
            "Cookie, Authorization".parse().expect("static vary policy"),
        );
    }
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("static content policy"),
    );
    response
}

/// Adds `X-Robots-Tag: noai, noimageai` to HTML responses while AI crawlers are blocked. If the
/// setting cannot be read, the header is sent: a page that might be blocked is not offered.
pub(crate) async fn no_ai_html(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = next.run(request).await;
    let is_html = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"text/html"));
    if is_html
        && crate::backend::api::crawlers::ai_crawlers_blocked(&state.pool)
            .await
            .unwrap_or(true)
    {
        response.headers_mut().insert(
            header::HeaderName::from_static("x-robots-tag"),
            "noai, noimageai".parse().expect("static robots policy"),
        );
    }
    response
}

pub(crate) async fn private_api_response(request: Request, next: Next) -> Response {
    if session::has_session_cookie(request.headers())
        || request.headers().contains_key(header::AUTHORIZATION)
    {
        private_sensitive_response(request, next).await
    } else {
        next.run(request).await
    }
}
