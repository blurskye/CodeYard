//! Streaming HTTP forwarding. A target must come from a validated directory record.
use crate::{
    body,
    routes::{web_node, ReadyCell},
};
use anyhow::{ensure, Result};
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderName, Request, Response, StatusCode},
};
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use http_body_util::Limited;
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::Semaphore;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Clone, Copy)]
pub struct Config {
    pub maximum_connections: usize,
    pub maximum_request_bytes: usize,
    pub maximum_response_bytes: u64,
    pub exchange_seconds: u64,
    pub stall_seconds: u64,
    pub minimum_rate: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            maximum_connections: 256,
            maximum_request_bytes: 2 * 1024 * 1024 * 1024,
            maximum_response_bytes: 64 * 1024 * 1024 * 1024,
            exchange_seconds: 1800,
            stall_seconds: 30,
            minimum_rate: 16 * 1024,
        }
    }
}
struct Owner {
    stop: CancellationToken,
    tasks: TaskTracker,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop.cancel();
        self.tasks.close();
    }
}
#[derive(Clone)]
pub struct Transport {
    client: reqwest::Client,
    slots: Arc<Semaphore>,
    config: Config,
    owner: Arc<Owner>,
    region: Option<header::HeaderValue>,
}
impl Transport {
    pub fn new(config: Config) -> Result<Self> {
        ensure!(
            (1..=4096).contains(&config.maximum_connections)
                && (1024..=2 * 1024 * 1024 * 1024).contains(&config.maximum_request_bytes)
                && (1024..=1024 * 1024 * 1024 * 1024).contains(&config.maximum_response_bytes)
                && (1..=3600).contains(&config.exchange_seconds),
            "Invalid HTTP forwarding limits"
        );
        body::Limits::new(config.stall_seconds, config.minimum_rate)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .http1_only()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(config.exchange_seconds))
            .pool_max_idle_per_host(8)
            .pool_idle_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            slots: Arc::new(Semaphore::new(config.maximum_connections)),
            config,
            owner: Arc::new(Owner {
                stop: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
            region: None,
        })
    }
    /// A public router names its region on every request it forwards, so cells can count
    /// where their accounts work from.
    pub fn in_region(mut self, region: &str) -> Result<Self> {
        codeyard_model::cells::validate_region(region)?;
        self.region = Some(header::HeaderValue::from_str(region)?);
        Ok(self)
    }
    pub async fn shutdown(&self) {
        self.owner.stop.cancel();
        self.owner.tasks.close();
        let _ = tokio::time::timeout(Duration::from_secs(10), self.owner.tasks.wait()).await;
    }
    /// Never retries a request: a failed write may already have committed in its cell.
    pub async fn forward(
        &self,
        cell: &ReadyCell,
        credential: &str,
        mut request: Request<Body>,
    ) -> Result<Response<Body>> {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| io::Error::other("Router is busy"))?;
        ensure!(!self.owner.stop.is_cancelled(), "Router is stopping");
        ensure!(
            request.uri().scheme().is_none() && request.uri().authority().is_none(),
            "Invalid request target"
        );
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", |path| path.as_str());
        ensure!(
            path.len() <= 16384 && path.starts_with('/'),
            "Invalid request target"
        );
        let url = format!(
            "{}{}",
            web_node(cell, credential)?.trim_end_matches('/'),
            path
        );
        let websocket = request
            .headers()
            .get(header::UPGRADE)
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"));
        let upgrade = websocket.then(|| hyper::upgrade::on(&mut request));
        let (mut parts, incoming) = request.into_parts();
        ensure!(
            incoming
                .size_hint()
                .upper()
                .is_none_or(|size| size <= self.config.maximum_request_bytes as u64),
            "Request is too large"
        );
        clean_headers(&mut parts.headers, websocket, true);
        if let Some(region) = &self.region {
            parts
                .headers
                .insert(codeyard_model::cells::REGION_HEADER, region.clone());
        }
        let incoming = SyncBody(Mutex::new(Limited::new(
            incoming,
            self.config.maximum_request_bytes,
        )));
        let upstream = self
            .client
            .request(parts.method, url)
            .headers(parts.headers)
            .body(reqwest::Body::wrap(incoming));
        let upstream = tokio::select! {
            _ = self.owner.stop.cancelled() => anyhow::bail!("Router is stopping"),
            result = upstream.send() => result.map_err(|_| io::Error::other("Cell is unavailable"))?,
        };
        let mut response = Response::builder().status(upstream.status());
        let mut headers = upstream.headers().clone();
        clean_headers(
            &mut headers,
            websocket && upstream.status() == StatusCode::SWITCHING_PROTOCOLS,
            false,
        );
        *response
            .headers_mut()
            .ok_or_else(|| io::Error::other("Invalid cell response"))? = headers;
        if upstream.status() == StatusCode::SWITCHING_PROTOCOLS {
            let downstream = upgrade.ok_or_else(|| io::Error::other("Unexpected cell upgrade"))?;
            let stop = self.owner.stop.clone();
            let seconds = self.config.exchange_seconds;
            let maximum = self.config.maximum_response_bytes;
            self.owner.tasks.spawn(async move {
                let exchange = async {
                    let (downstream, upstream) = tokio::try_join!(
                        async { downstream.await.map_err(io::Error::other) },
                        async { upstream.upgrade().await.map_err(io::Error::other) }
                    )?;
                    let mut downstream = hyper_util::rt::TokioIo::new(downstream);
                    let mut upstream = upstream;
                    let (dr, mut dw) = tokio::io::split(&mut downstream);
                    let (ur, mut uw) = tokio::io::split(&mut upstream);
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let pump = async {
                        let sent = tokio::io::copy(&mut dr.take(maximum + 1), &mut uw).await?;
                        if sent > maximum { return Err(io::Error::other("Upgrade is too large")); }
                        uw.shutdown().await
                    };
                    let back = async {
                        let sent = tokio::io::copy(&mut ur.take(maximum + 1), &mut dw).await?;
                        if sent > maximum { return Err(io::Error::other("Upgrade is too large")); }
                        dw.shutdown().await
                    };
                    tokio::try_join!(pump, back)?;
                    Ok::<_, io::Error>(())
                };
                tokio::select! { _ = stop.cancelled() => (), _ = tokio::time::timeout(Duration::from_secs(seconds), exchange) => () }
                drop(permit);
            });
            return Ok(response.body(Body::empty())?);
        }
        ensure!(
            upstream
                .content_length()
                .is_none_or(|size| size <= self.config.maximum_response_bytes),
            "Cell response is too large"
        );
        let stop = self.owner.stop.clone();
        let maximum = self.config.maximum_response_bytes;
        let stream = futures_util::stream::try_unfold(
            (upstream, permit, 0_u64, stop),
            move |(mut upstream, permit, received, stop)| async move {
                let chunk = tokio::select! {
                    _ = stop.cancelled() => return Err(io::Error::other("Router is stopping")),
                    chunk = upstream.chunk() => chunk.map_err(|_| io::Error::other("Cell response interrupted"))?,
                };
                let Some(chunk) = chunk else {
                    return Ok(None);
                };
                let received = received
                    .checked_add(chunk.len() as u64)
                    .filter(|size| *size <= maximum)
                    .ok_or_else(|| io::Error::other("Cell response is too large"))?;
                Ok(Some((chunk, (upstream, permit, received, stop))))
            },
        );
        Ok(response.body(body::download_body_with(
            body::Limits::new(self.config.stall_seconds, self.config.minimum_rate)?,
            stream,
        ))?)
    }
}

// Axum's body is Send but not Sync; the mutex makes the HTTP-body adapter Sync.
// Hyper polls it through an exclusive reference and never holds a lock across await.
struct SyncBody(Mutex<Limited<Body>>);
impl HttpBody for SyncBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match self.get_mut().0.get_mut() {
            Ok(body) => Pin::new(body).poll_frame(context),
            Err(_) => Poll::Ready(Some(Err(Box::new(io::Error::other(
                "Request body unavailable",
            ))))),
        }
    }
    fn size_hint(&self) -> SizeHint {
        self.0
            .lock()
            .map(|body| body.size_hint())
            .unwrap_or_default()
    }
}
fn clean_headers(headers: &mut HeaderMap, websocket: bool, request: bool) {
    let nominated: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
    if request {
        let internal: Vec<HeaderName> = headers
            .keys()
            .filter(|name| name.as_str().starts_with("x-codeyard-"))
            .cloned()
            .collect();
        for name in internal {
            headers.remove(name);
        }
    }
    if websocket {
        headers.insert(
            header::CONNECTION,
            header::HeaderValue::from_static("upgrade"),
        );
        headers.insert(
            header::UPGRADE,
            header::HeaderValue::from_static("websocket"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        routing::{get, post},
        Router,
    };
    use codeyard_model::cells::{Cell, State};
    use codeyard_types::cells::Prefix;
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::task::AbortOnDropHandle;

    #[tokio::test]
    async fn real_http_preserves_auth_redirects_and_bounds_both_streams() {
        let writes = Arc::new(AtomicUsize::new(0));
        let count = writes.clone();
        let app = Router::new()
            .route(
                "/headers",
                get(|headers: HeaderMap| async move {
                    assert!(!headers.contains_key("x-codeyard-client"));
                    assert!(!headers.contains_key("x-secret"));
                    assert_eq!(headers[header::AUTHORIZATION], "Bearer cell-token");
                    "current copy"
                }),
            )
            .route(
                "/write",
                post(move || async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    "written"
                }),
            )
            .route(
                "/redirect",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, "https://example.invalid/never-fetch")],
                    )
                }),
            )
            .route(
                "/large",
                get(|| async {
                    Body::from_stream(futures_util::stream::iter([
                        Ok::<_, io::Error>(Bytes::from(vec![b'a'; 800])),
                        Ok(Bytes::from(vec![b'b'; 800])),
                    ]))
                }),
            )
            .route(
                "/interrupted",
                get(|| async {
                    Body::from_stream(futures_util::stream::iter([
                        Ok(Bytes::from_static(b"partial")),
                        Err(io::Error::other("Disk failed")),
                    ]))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap()
        }));
        let cell = ReadyCell::new(Cell {
            id: Prefix::ZERO,
            name: "zero".into(),
            region: "a".into(),
            state: State::Open,
            web_urls: vec![format!("http://{address}")],
            ssh_addresses: vec![],
            rpc_addresses: vec![],
            node_keys: vec!["a".repeat(64)],
            database_bytes: 0,
            write_rate: 0,
            maximum_database_bytes: 100,
            maximum_write_rate: 100,
            read_regions: Vec::new(),
        })
        .unwrap();
        let transport = Transport::new(Config {
            maximum_request_bytes: 1024,
            maximum_response_bytes: 1024,
            ..Config::default()
        })
        .unwrap();
        let request = Request::builder()
            .uri("/headers")
            .header(header::AUTHORIZATION, "Bearer cell-token")
            .header(header::CONNECTION, "x-secret")
            .header("x-secret", "discard")
            .header("x-codeyard-client", "spoofed")
            .body(Body::empty())
            .unwrap();
        let response = transport
            .forward(&cell, "cell-token", request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "current copy"
        );
        let response = transport
            .forward(
                &cell,
                "",
                Request::builder()
                    .uri("/redirect")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://example.invalid/never-fetch"
        );
        drop(response);
        let request = Request::builder()
            .method("POST")
            .uri("/write")
            .body(Body::from(vec![0; 1025]))
            .unwrap();
        assert!(transport.forward(&cell, "", request).await.is_err());
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        let request = Request::builder()
            .uri("http://example.invalid/headers")
            .body(Body::empty())
            .unwrap();
        assert!(transport.forward(&cell, "", request).await.is_err());
        for path in ["/large", "/interrupted"] {
            if let Ok(response) = transport
                .forward(
                    &cell,
                    "",
                    Request::builder().uri(path).body(Body::empty()).unwrap(),
                )
                .await
            {
                assert!(response.into_body().collect().await.is_err());
            }
        }
        server.abort();
        let _ = server.await;
        assert!(transport
            .forward(&cell, "", Request::new(Body::empty()))
            .await
            .is_err());
        transport.shutdown().await;
    }
}
