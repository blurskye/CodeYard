//! Node access and private representation middleware.
use crate::error::ApiError;
use crate::session::get_current_user;
use axum::body::HttpBody;
use axum::extract::ConnectInfo;
use axum::extract::MatchedPath;
use axum::extract::Request;
use axum::http::header;
use axum::http::Method;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Extension;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_types::http_config::WebConfig;
use std::collections::HashSet;
use std::io;
use std::net::IpAddr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Sets the header that `codeyard_identity::client_address::client_address` reads, and drops any value the client sent.
/// The address is the connection's, or, when the connection comes from a trusted proxy, the
/// client that proxy forwarded for. IPv6 clients are told apart by their /64 network.
pub async fn stamp_client_address(
    Extension(config): Extension<Arc<WebConfig>>,
    mut request: Request,
    next: Next,
) -> Response {
    request
        .headers_mut()
        .remove(codeyard_identity::client_address::CLIENT_HEADER);
    // Only a router, as a trusted proxy, names the region a request came through.
    let region = request
        .headers_mut()
        .remove(codeyard_types::cells::REGION_HEADER)
        .filter(|value| {
            value
                .to_str()
                .is_ok_and(|value| codeyard_types::cells::validate_region(value).is_ok())
        })
        .filter(|_| from_trusted_proxy(&request, &config.trusted_proxies));
    if let Some(Ok(value)) = client_key(&request, &config.trusted_proxies)
        .map(|client| client.parse::<header::HeaderValue>())
    {
        request
            .headers_mut()
            .insert(codeyard_identity::client_address::CLIENT_HEADER, value);
    }
    if let Some(region) = region {
        request
            .headers_mut()
            .insert(codeyard_types::cells::REGION_HEADER, region);
    }
    next.run(request).await
}

fn from_trusted_proxy(request: &Request, trusted_proxies: &HashSet<IpAddr>) -> bool {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(peer)| trusted_proxies.contains(&peer.ip().to_canonical()))
}

/// The token and browser-session proofs a request carries, for any home cell.
fn credential_proofs(
    headers: &axum::http::HeaderMap,
) -> [Option<codeyard_database::cells::authorization::Proof>; 2] {
    use codeyard_database::cells::authorization::Proof;
    let token = if headers.contains_key(header::AUTHORIZATION) {
        match codeyard_router::selection::Credential::from_headers(headers, "session_id") {
            Ok(Some(hint)) => Some(Proof::Token {
                home: hint.prefix,
                hash: codeyard_support::token_hash::token_hash(hint.value()),
            }),
            _ => None,
        }
    } else {
        None
    };
    let session = crate::session::extract_session(headers)
        .map(|(home, value)| Proof::Session { home, value });
    [token, session]
}

/// Counts an authenticated request a router forwarded, under the router's region, for the
/// account its credential belongs to. Anonymous requests and rejected credentials are not
/// counted; a token counts before a browser session, as authentication prefers it.
pub async fn count_regional_requests(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let codeyard_database::DbPool::Postgres(pool) = &state.pool else {
        return next.run(request).await;
    };
    let region = request
        .headers()
        .get(codeyard_types::cells::REGION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (Some(region), true) = (region, pool.directory().is_some()) else {
        return next.run(request).await;
    };
    let proof = credential_proofs(request.headers())
        .into_iter()
        .flatten()
        .next();
    let response = next.run(request).await;
    if response.status() == StatusCode::UNAUTHORIZED {
        return response;
    }
    let Some(proof) = proof else {
        return response;
    };
    let Ok(key) = serde_json::to_string(&proof) else {
        return response;
    };
    let key = codeyard_support::token_hash::token_hash(&key);
    let counts = pool.request_counts();
    let user = match counts.identity(&key) {
        Some(user) => user,
        None => {
            let user =
                tokio::time::timeout(Duration::from_secs(2), credential_account(&state, &proof))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .flatten();
            counts.remember(key, user);
            user
        }
    };
    if let Some(user) = user {
        counts.add(user, &region, chrono::Utc::now().date_naive());
    }
    response
}

async fn credential_account(
    state: &AppState,
    proof: &codeyard_database::cells::authorization::Proof,
) -> anyhow::Result<Option<i64>> {
    if matches!(
        proof,
        codeyard_database::cells::authorization::Proof::Ssh { .. }
    ) {
        return Ok(None);
    }
    let local = codeyard_identity::cells::Local {
        pool: state.pool.clone(),
    };
    // A foreign answer is kept only inside an operation scope; this runs outside the request's.
    let answer = codeyard_database::cells::authorization::scope(local.resolve(proof)).await?;
    Ok(answer.map(|answer| answer.user.id))
}

/// Prime home answers before any adapter looks up a foreign token or browser session.
/// Header parsing is shared with the router; resource authority stays in native handlers.
pub async fn foreign_authorization(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if state.pool.foreign_authorization().is_none() {
        return next.run(request).await;
    }
    use codeyard_database::cells::authorization::authorize;
    let local = codeyard_identity::cells::Local {
        pool: state.pool.clone(),
    };
    // Another cell answers for a foreign credential and for a token whose account moved.
    let mut foreign = Vec::new();
    for proof in credential_proofs(request.headers()).into_iter().flatten() {
        match local.remote(&proof).await {
            Ok(true) => foreign.push(proof),
            Ok(false) => {}
            Err(_) => return codeyard_types::github_error::Error::unavailable(()).into_response(),
        }
    }
    codeyard_database::cells::authorization::scope(async {
        for proof in &foreign {
            if authorize(&state.pool, proof).await.is_err() {
                return codeyard_types::github_error::Error::unavailable(()).into_response();
            }
        }
        let response = next.run(request).await;
        if codeyard_database::cells::authorization::ensure_current().is_err() {
            // Long operations (including forwarding a fork) may outlive admission.
            // Ask the primary again before exposing their response; never renew an
            // old snapshot just because the downstream work completed.
            for proof in &foreign {
                if !matches!(authorize(&state.pool, proof).await, Ok(Some(_))) {
                    return codeyard_types::github_error::Error::unavailable(()).into_response();
                }
            }
            if codeyard_database::cells::authorization::ensure_current().is_err() {
                return codeyard_types::github_error::Error::unavailable(()).into_response();
            }
        }
        response
    })
    .await
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
        .is_some_and(|matched| {
            let path = matched
                .as_str()
                .strip_prefix(crate::shared::repos::LEGACY_PREFIX)
                .unwrap_or(matched.as_str());
            (*request.method() == Method::PUT
                && matches!(
                    path,
                    crate::shared::files::CONTENT_ROUTE
                        | crate::shared::github::code_scanning::SARIF_ACTION_ROUTE
                ))
                || (*request.method() == Method::POST
                    && matches!(
                        path,
                        crate::shared::repos::code::upload::ROUTE
                            | crate::shared::repos::releases::ASSET_ROUTE
                            | crate::shared::github::git_objects::BLOB_ROUTE
                            | crate::shared::github::code_scanning::SARIF_ROUTE
                            | crate::shared::github::code_scanning::databases::UPLOAD_ROUTE
                            | crate::shared::github::code_scanning::databases::API_UPLOAD_ROUTE
                            | crate::shared::github::code_scanning::databases::HOST_UPLOAD_ROUTE
                            | crate::shared::github::RELEASE_ASSET_UPLOAD_ROUTE
                            | crate::shared::github::GISTS_ROUTE
                            | crate::shared::github::GIST_ROUTE
                    ))
                || (*request.method() == Method::PATCH && path == crate::shared::github::GIST_ROUTE)
        })
}

/// Bodies up to this size are read without a slot: a client that stalls on one holds nothing
/// that another client needs.
const SMALL_BODY: u64 = 64 * 1024;
const MAX_BODY: u64 = 4 * 1024 * 1024;

/// Reads the whole body of every POST, PUT, PATCH and DELETE request before its handler runs,
/// within 4 MiB and 30 seconds. A body that is not known to be small takes a slot from the
/// authenticated or the anonymous pool, 8 each, while it is read. Upload routes are exempt.
pub async fn bounded_form_body(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    // Nested legacy routes have not selected their leaf yet. Their own layer applies this
    // same policy after matching, so streaming uploads remain streaming, not 4 MiB forms.
    if request
        .extensions()
        .get::<MatchedPath>()
        .is_some_and(|path| {
            path.as_str()
                .starts_with(crate::shared::repos::LEGACY_PREFIX)
        })
    {
        return next.run(request).await;
    }
    bounded_mutation_body(state, request, next).await
}
pub async fn bounded_legacy_repository_body(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    bounded_mutation_body(state, request, next).await
}
async fn bounded_mutation_body(state: Arc<AppState>, request: Request, next: Next) -> Response {
    if !matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) || streams_upload(&request)
    {
        return next.run(request).await;
    }
    let maximum = if request.uri().path().ends_with("/_forms/write-file") {
        crate::shared::repos::code::contents::EDITOR_FORM_BODY_LIMIT as u64
    } else {
        MAX_BODY
    };
    let declared = request.body().size_hint().upper();
    if declared.is_some_and(|size| size > maximum) {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let permit = if declared.is_some_and(|size| size <= SMALL_BODY) {
        None
    } else {
        let mut authenticated =
            match crate::session::get_optional_user(&state.pool, request.headers()).await {
                Ok(user) => user.is_some(),
                // Password-change forms still need to reach their own session check.
                Err((StatusCode::PRECONDITION_REQUIRED, _)) => true,
                Err(error) => return error.into_response(),
            };
        if !authenticated
            && (request.uri().path().starts_with("/api/v3/")
                || request.uri().path() == "/api/graphql")
        {
            authenticated = match codeyard_identity::github_auth::authenticate(
                &state,
                request.headers(),
                false,
            )
            .await
            {
                Ok(auth) => auth.is_some(),
                Err(error) => return error.into_response(),
            };
        }
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
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(
        Duration::from_secs(30),
        axum::body::to_bytes(body, maximum as usize),
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

/// How long a download or search waits for a free slot before the request is refused.
const SLOT_WAIT: Duration = Duration::from_secs(3);

/// Waits briefly for a slot so a short burst queues instead of failing at once.
pub async fn wait_for_slot(
    slots: &Arc<tokio::sync::Semaphore>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    tokio::time::timeout(SLOT_WAIT, slots.clone().acquire_owned())
        .await
        .ok()?
        .ok()
}

/// Waits briefly for a slot of a fixed pool, like `wait_for_slot`.
pub async fn wait_for_static_slot(
    slots: &'static tokio::sync::Semaphore,
) -> Option<tokio::sync::SemaphorePermit<'static>> {
    tokio::time::timeout(SLOT_WAIT, slots.acquire())
        .await
        .ok()?
        .ok()
}

pub use codeyard_router::body::download_body;

pub fn buffered_download_body(
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

pub async fn require_node_access(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let node_config =
        match codeyard_db_placement::node_config::get_node_configuration(&state.pool).await {
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

pub async fn private_sensitive_response(request: Request, next: Next) -> Response {
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
pub async fn no_ai_html(
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
        && crate::shared::crawlers::ai_crawlers_blocked(&state.pool)
            .await
            .unwrap_or(true)
    {
        response.headers_mut().append(
            header::HeaderName::from_static("x-robots-tag"),
            "noai, noimageai".parse().expect("static robots policy"),
        );
    }
    response
}

pub async fn private_api_response(request: Request, next: Next) -> Response {
    private_sensitive_response(request, next).await
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
