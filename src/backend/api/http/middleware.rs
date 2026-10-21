//! Node access and private representation middleware.
use super::{
    config::WebConfig,
    error::ApiError,
    session::{self, get_current_user},
};
use crate::backend::api::{
    files,
    repos::{code::upload, releases},
};
use crate::backend::{app::AppState, db};
use axum::{
    body::HttpBody,
    extract::{ConnectInfo, MatchedPath, Request},
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json,
};
use futures_util::{Stream, StreamExt};
use std::{
    collections::HashSet,
    io,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

/// Sets the header that `session::client_address` reads, and drops any value the client sent.
/// The address is the connection's, or, when the connection comes from a trusted proxy, the
/// client that proxy forwarded for. IPv6 clients are told apart by their /64 network.
pub(crate) async fn stamp_client_address(
    Extension(config): Extension<Arc<WebConfig>>,
    mut request: Request,
    next: Next,
) -> Response {
    request.headers_mut().remove(session::CLIENT_HEADER);
    if let Some(Ok(value)) = client_key(&request, &config.trusted_proxies)
        .map(|client| client.parse::<header::HeaderValue>())
    {
        request.headers_mut().insert(session::CLIENT_HEADER, value);
    }
    next.run(request).await
}

fn client_key(request: &Request, trusted_proxies: &HashSet<IpAddr>) -> Option<String> {
    let ConnectInfo(peer) = request.extensions().get::<ConnectInfo<SocketAddr>>()?;
    let mut client = peer.ip().to_canonical();
    if trusted_proxies.contains(&client) {
        let forwarded: Vec<&str> = request
            .headers()
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .collect();
        // Each proxy appends the address it saw, so the client is the last one that is not a
        // trusted proxy itself.
        for entry in forwarded.into_iter().rev() {
            let Ok(address) = entry.trim().parse::<IpAddr>() else {
                break;
            };
            client = address.to_canonical();
            if !trusted_proxies.contains(&client) {
                break;
            }
        }
    }
    Some(match client {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => {
            Ipv6Addr::from(u128::from(address) & !u128::from(u64::MAX)).to_string()
        }
    })
}

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

/// Bodies up to this size are read without a slot: a client that stalls on one holds nothing
/// that another client needs.
const SMALL_BODY: u64 = 64 * 1024;
const MAX_BODY: u64 = 4 * 1024 * 1024;

/// Reads the whole body of every POST, PUT, PATCH and DELETE request before its handler runs,
/// within 4 MiB and 30 seconds. A body that is not known to be small takes a slot from the
/// authenticated or the anonymous pool, 8 each, while it is read. Upload routes are exempt.
pub(crate) async fn bounded_form_body(
    Extension(state): Extension<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    request.headers_mut().remove("x-codeyard-submitted");
    request.headers_mut().remove("x-codeyard-draft-submission");
    if request.method() == Method::POST {
        request
            .headers_mut()
            .insert("x-codeyard-submitted", "true".parse().unwrap());
    }
    if !matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) || streams_upload(&request)
    {
        return next.run(request).await;
    }
    let declared = request.body().size_hint().upper();
    if declared.is_some_and(|size| size > MAX_BODY) {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let permit = if declared.is_some_and(|size| size <= SMALL_BODY) {
        None
    } else {
        let authenticated = match session::get_optional_user(&state.pool, request.headers()).await {
            Ok(user) => user.is_some(),
            // Password-change forms still need to reach their own session check.
            Err((StatusCode::PRECONDITION_REQUIRED, _)) => true,
            Err(error) => return error.into_response(),
        };
        let reads = if authenticated {
            &state.form_reads
        } else {
            &state.anonymous_form_reads
        };
        let Ok(permit) = reads.try_acquire() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        Some(permit)
    };
    let (mut parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(
        Duration::from_secs(30),
        axum::body::to_bytes(body, MAX_BODY as usize),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    drop(permit);
    if parts
        .headers
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| {
            value
                .as_bytes()
                .starts_with(b"application/x-www-form-urlencoded")
        })
    {
        if let Some(token) = url::form_urlencoded::parse(&bytes)
            .find_map(|(name, value)| (name == "draft_submission").then_some(value.into_owned()))
        {
            if valid_submission_token(&token) {
                parts
                    .headers
                    .insert("x-codeyard-draft-submission", token.parse().unwrap());
            }
        }
    }
    next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
        .await
}

pub(super) fn valid_submission_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 100
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
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
        Some(config) => (config.setup_completed, config.is_private()),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request(peer: &str, forwarded: &[&str]) -> Request {
        let mut request = Request::new(axum::body::Body::empty());
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        for value in forwarded {
            request
                .headers_mut()
                .append("x-forwarded-for", value.parse().unwrap());
        }
        request
    }

    #[test]
    fn forwarded_for_is_believed_only_from_trusted_proxies() {
        let trusted = HashSet::from(["10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap()]);
        let key = |peer, forwarded: &[&str]| client_key(&request(peer, forwarded), &trusted);
        assert_eq!(
            key("203.0.113.5:1000", &["198.51.100.1"]).as_deref(),
            Some("203.0.113.5")
        );
        assert_eq!(key("10.0.0.1:1000", &[]).as_deref(), Some("10.0.0.1"));
        assert_eq!(
            key("10.0.0.1:1000", &["198.51.100.9, 198.51.100.1, 10.0.0.2"]).as_deref(),
            Some("198.51.100.1")
        );
    }

    #[test]
    fn ipv6_clients_share_one_budget_per_64() {
        let key = client_key(&request("[2001:db8:1:2:3:4:5:6]:80", &[]), &HashSet::new());
        assert_eq!(key.as_deref(), Some("2001:db8:1:2::"));
    }

    #[test]
    fn a_request_without_a_socket_has_no_client() {
        let request = Request::new(axum::body::Body::empty());
        assert_eq!(client_key(&request, &HashSet::new()), None);
    }
}
