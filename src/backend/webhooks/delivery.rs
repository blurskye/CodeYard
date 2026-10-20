//! One webhook request: signed, sent with a timeout and no redirects, retried on server
//! errors and timeouts, and recorded with what was sent and what came back.
use super::outbound::{self, Refusal};
use crate::backend::crypto::SecretEncryptor;
use crate::backend::db::models::{Webhook, WebhookDelivery};
use crate::backend::db::webhooks::{self as hooks, NewDelivery};
use crate::backend::db::DbPool;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use url::{Host, Url};

const TIMEOUT: Duration = Duration::from_secs(10);
/// Waits before the first, second and third retry.
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
const BODY_LIMIT: usize = 64 * 1024;
const HEADERS_LIMIT: usize = 8 * 1024;

/// What one delivery sends. The body is already encoded for `content_type`.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub event: String,
    pub action: Option<String>,
    pub guid: String,
    pub content_type: String,
    pub body: String,
    /// Automatic events obey the active flag and subscription; manual redelivery does not.
    pub automatic: bool,
}

enum Attempt {
    Responded {
        status: u16,
        headers: String,
        body: String,
    },
    Failed {
        error: &'static str,
        retry: bool,
    },
}

/// Sends `outgoing` to the hook, retrying up to three times, and records the final attempt.
/// Errors are logged, never returned: a delivery is a background effect.
pub async fn send(pool: &DbPool, hook: &Webhook, outgoing: &Outgoing) {
    let record = match transmit(pool, hook, outgoing).await {
        Ok(Some(record)) => record,
        Ok(None) => return,
        Err(error) => failed_record(outgoing, 0, error),
    };
    if let Err(error) = hooks::insert_delivery(pool, hook.id, &record).await {
        tracing::warn!(webhook_id = hook.id, %error, "could not record a webhook delivery");
    }
}

/// Resends a recorded delivery to the hook's current URL, as a new delivery.
pub fn redelivery(recorded: &WebhookDelivery) -> Outgoing {
    let content_type = recorded
        .request_headers
        .lines()
        .find_map(|line| line.strip_prefix("Content-Type: "))
        .unwrap_or("application/json")
        .to_owned();
    Outgoing {
        event: recorded.event.clone(),
        action: recorded.action.clone(),
        guid: uuid::Uuid::new_v4().to_string(),
        content_type,
        body: recorded.request_body.clone(),
        automatic: false,
    }
}

async fn current_hook(
    pool: &DbPool,
    hook: &Webhook,
    outgoing: &Outgoing,
) -> Result<bool, &'static str> {
    let owner = match (hook.repo_id, hook.org_id) {
        (Some(id), None) => hooks::HookOwner::Repository(id),
        (None, Some(id)) => hooks::HookOwner::Organization(id),
        _ => return Ok(false),
    };
    let mut retries = RETRY_DELAYS.iter();
    let current = loop {
        match hooks::get_webhook(pool, owner, hook.id).await {
            Ok(Some(current)) => break current,
            Ok(None) => return Ok(false),
            Err(error) => {
                tracing::warn!(webhook_id = hook.id, %error, "could not recheck a queued webhook");
                match retries.next() {
                    Some(delay) => tokio::time::sleep(*delay).await,
                    None => return Err("Webhook storage is unavailable"),
                }
            }
        }
    };
    Ok(current.payload_url == hook.payload_url
        && current.content_type == hook.content_type
        && current.encrypted_secret == hook.encrypted_secret
        && current.secret_nonce == hook.secret_nonce
        && current.ssl_verify == hook.ssl_verify
        && current.updated_at == hook.updated_at
        && (!outgoing.automatic
            || (current.active && super::events::subscribes(&current.events, &outgoing.event))))
}

async fn transmit(
    pool: &DbPool,
    hook: &Webhook,
    outgoing: &Outgoing,
) -> Result<Option<NewDelivery>, &'static str> {
    if !current_hook(pool, hook, outgoing).await? {
        return Ok(None);
    }
    let secret = open_secret(hook)?;
    let url = outbound::validate_url(&hook.payload_url, outbound::allow_private())
        .map_err(refusal_text)?;
    let mut headers = vec![
        ("Content-Type", outgoing.content_type.clone()),
        ("User-Agent", user_agent()),
        ("X-GitHub-Event", outgoing.event.clone()),
        ("X-GitHub-Delivery", outgoing.guid.clone()),
    ];
    if let Some(secret) = &secret {
        headers.push((
            "X-Hub-Signature-256",
            format!(
                "sha256={}",
                hex::encode(hmac_sha256(secret, outgoing.body.as_bytes()))
            ),
        ));
    }
    let mut retries = RETRY_DELAYS.iter();
    let attempt = loop {
        if !current_hook(pool, hook, outgoing).await? {
            return Ok(None);
        }
        let slot = super::SLOTS
            .acquire()
            .await
            .map_err(|_| "Webhook sends are stopped")?;
        let started_attempt = Instant::now();
        let attempt = attempt(&url, hook.ssl_verify, &headers, &outgoing.body).await;
        let elapsed = started_attempt.elapsed();
        drop(slot);
        let retry = match &attempt {
            Attempt::Responded { status, .. } => *status >= 500,
            Attempt::Failed { retry, .. } => *retry,
        };
        match retries.next() {
            Some(delay) if retry => {
                tokio::time::sleep(*delay).await;
                continue;
            }
            _ => break (attempt, elapsed),
        }
    };
    let (attempt, elapsed) = attempt;
    let request_headers = join_headers(headers.iter().map(|(name, value)| (*name, value.as_str())));
    let duration_ms = i32::try_from(elapsed.as_millis()).unwrap_or(i32::MAX);
    Ok(Some(match attempt {
        Attempt::Responded {
            status,
            headers,
            body,
        } => NewDelivery {
            guid: outgoing.guid.clone(),
            event: outgoing.event.clone(),
            action: outgoing.action.clone(),
            status_code: Some(i32::from(status)),
            duration_ms,
            error: None,
            request_headers,
            request_body: outgoing.body.clone(),
            response_headers: Some(headers),
            response_body: Some(body),
        },
        Attempt::Failed { error, .. } => NewDelivery {
            duration_ms,
            request_headers,
            ..failed_record(outgoing, duration_ms, error)
        },
    }))
}

async fn attempt(url: &Url, ssl_verify: bool, headers: &[(&str, String)], body: &str) -> Attempt {
    bounded_attempt(attempt_inner(url, ssl_verify, headers, body)).await
}

async fn bounded_attempt(future: impl std::future::Future<Output = Attempt>) -> Attempt {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .unwrap_or(Attempt::Failed {
            error: "The request timed out after 10 seconds",
            retry: true,
        })
}

async fn attempt_inner(
    url: &Url,
    ssl_verify: bool,
    headers: &[(&str, String)],
    body: &str,
) -> Attempt {
    let addresses = match outbound::resolve(url, outbound::allow_private()).await {
        Ok(addresses) => addresses,
        Err(refusal) => {
            return Attempt::Failed {
                error: refusal_text(refusal),
                retry: false,
            }
        }
    };
    let client = match client(url, &addresses, ssl_verify) {
        Ok(client) => client,
        Err(_) => {
            return Attempt::Failed {
                error: "The HTTP client could not start",
                retry: false,
            }
        }
    };
    let mut request = client.post(url.as_str()).body(body.to_owned());
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    let mut response = match request.send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return Attempt::Failed {
                error: "The request timed out after 10 seconds",
                retry: true,
            }
        }
        Err(error) if error.is_connect() => {
            return Attempt::Failed {
                error: "The connection failed",
                retry: true,
            }
        }
        Err(_) => {
            return Attempt::Failed {
                error: "The request failed",
                retry: false,
            }
        }
    };
    let status = response.status().as_u16();
    let headers = join_headers(
        response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_str().unwrap_or(""))),
    );
    let body = match read_limited(&mut response).await {
        Ok(bytes) => response_text(&bytes),
        Err(_) => String::new(),
    };
    Attempt::Responded {
        status,
        headers,
        body,
    }
}

fn response_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\0', "\\0")
}

/// The client for one attempt. A domain name is pinned to the addresses that were just
/// checked, so the connection cannot reach a different host than the one that passed. A proxy
/// from the environment would receive the name instead, so proxies are not used.
fn client(
    url: &Url,
    addresses: &[SocketAddr],
    ssl_verify: bool,
) -> reqwest::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(!ssl_verify)
        .user_agent(user_agent());
    if let Some(Host::Domain(name)) = url.host() {
        builder = builder.resolve_to_addrs(name, addresses);
    }
    builder.build()
}

async fn read_limited(response: &mut reqwest::Response) -> Result<Vec<u8>, reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = BODY_LIMIT - body.len();
        if chunk.len() >= room {
            body.extend_from_slice(&chunk[..room]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn open_secret(hook: &Webhook) -> Result<Option<Vec<u8>>, &'static str> {
    let (Some(ciphertext), Some(nonce)) = (&hook.encrypted_secret, &hook.secret_nonce) else {
        return Ok(None);
    };
    let encryptor = SecretEncryptor::configured_from_env()
        .ok()
        .flatten()
        .ok_or("The secret key is not configured")?;
    encryptor
        .decrypt(ciphertext, nonce)
        .map(Some)
        .map_err(|_| "The secret could not be opened")
}

fn refusal_text(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Invalid(text) => text,
        Refusal::Blocked => "The address is private and is not allowed",
        Refusal::Unreachable => "The host name did not resolve",
    }
}

fn user_agent() -> String {
    format!("Codeyard-Hookshot/{}", env!("CARGO_PKG_VERSION"))
}

/// Headers as `Name: value` lines, cut to a fixed size.
fn join_headers<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let mut text = String::new();
    for (name, value) in pairs {
        let line = format!("{name}: {value}\n");
        if text.len() + line.len() > HEADERS_LIMIT {
            break;
        }
        text.push_str(&line);
    }
    text
}

fn failed_record(outgoing: &Outgoing, duration_ms: i32, error: &str) -> NewDelivery {
    NewDelivery {
        guid: outgoing.guid.clone(),
        event: outgoing.event.clone(),
        action: outgoing.action.clone(),
        status_code: None,
        duration_ms,
        error: Some(error.to_owned()),
        request_headers: format!("Content-Type: {}\n", outgoing.content_type),
        request_body: outgoing.body.clone(),
        response_headers: None,
        response_body: None,
    }
}

/// HMAC-SHA256 from SHA-256 (RFC 2104), so no new dependency is needed.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner_pad: Vec<u8> = block.iter().map(|byte| byte ^ 0x36).collect();
    let outer_pad: Vec<u8> = block.iter().map(|byte| byte ^ 0x5c).collect();
    let inner = Sha256::new()
        .chain_update(&inner_pad)
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(&outer_pad)
        .chain_update(inner)
        .finalize()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timeout_covers_the_whole_attempt_and_is_retryable() {
        let result = bounded_attempt(std::future::pending()).await;
        assert!(matches!(result, Attempt::Failed { retry: true, .. }));
    }

    #[test]
    fn binary_response_text_is_database_safe() {
        assert_eq!(response_text(b"ok\0\xff"), "ok\\0\u{fffd}");
    }

    #[test]
    fn preflight_failure_preserves_content_type() {
        let outgoing = Outgoing {
            event: "push".into(),
            action: None,
            guid: "id".into(),
            content_type: "application/x-www-form-urlencoded".into(),
            body: "payload=%7B%7D".into(),
            automatic: true,
        };
        let failed = failed_record(&outgoing, 0, "The secret key is not configured");
        assert_eq!(
            failed.request_headers,
            format!("Content-Type: {}\n", outgoing.content_type)
        );
        assert_eq!(failed.request_body, outgoing.body);
    }

    #[test]
    fn hmac_matches_rfc_4231_vectors() {
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let long_key = [0xaa; 131];
        assert_eq!(
            hex::encode(hmac_sha256(
                &long_key,
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn redelivery_reuses_the_recorded_body_and_content_type() {
        let recorded = WebhookDelivery {
            id: 1,
            webhook_id: 2,
            guid: "old".into(),
            event: "push".into(),
            action: None,
            status_code: Some(500),
            duration_ms: 3,
            error: None,
            request_headers:
                "Content-Type: application/x-www-form-urlencoded\nX-GitHub-Event: push\n".into(),
            request_body: "payload=%7B%7D".into(),
            response_headers: None,
            response_body: None,
            delivered_at: chrono::Utc::now(),
        };
        let outgoing = redelivery(&recorded);
        assert_eq!(outgoing.content_type, "application/x-www-form-urlencoded");
        assert_eq!(outgoing.body, "payload=%7B%7D");
        assert_eq!(outgoing.event, "push");
        assert_ne!(outgoing.guid, "old");
    }

    /// reqwest reads the proxy variables when a client is built, so the client is built in a child
    /// process that starts with them set. A request to a pinned name must reach the pinned address,
    /// not the proxy.
    #[test]
    fn webhook_requests_do_not_use_a_proxy() {
        const CHILD: &str = "CODEYARD_WEBHOOK_PROXY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("webhook_requests_do_not_use_a_proxy")
                .env(CHILD, "1")
                // Nothing listens on the discard port, so a request sent to the proxy fails.
                .env("HTTP_PROXY", "http://127.0.0.1:9")
                .env("http_proxy", "http://127.0.0.1:9")
                .env_remove("NO_PROXY")
                .env_remove("no_proxy")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = [0u8; 4096];
                    let read = stream.read(&mut request).await.unwrap();
                    assert!(read > 0, "the request reached the pinned address");
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                });
                let url =
                    Url::parse(&format!("http://hook.example.test:{}/", address.port())).unwrap();
                let client = client(&url, &[address], true).unwrap();
                let response = client.post(url).body("{}").send().await.unwrap();
                assert_eq!(response.status(), reqwest::StatusCode::OK);
            });
    }
}
