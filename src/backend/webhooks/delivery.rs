//! One webhook request: signed, sent with a timeout and no redirects, retried on server
//! errors and timeouts, and recorded with what was sent and what came back.

use crate::backend::webhooks::outbound::Refusal;
use base64::Engine as _;
use codeyard_db_core::models::Webhook;
use codeyard_db_core::models::WebhookDelivery;
use codeyard_db_core::queries::webhooks::NewDelivery;
use codeyard_db_core::DbPool;
use codeyard_db_repos::webhooks as hooks;
use codeyard_support::crypto::hmac_sha256;
use codeyard_support::secrets::SecretEncryptor;
use hmac::{Hmac, Mac};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use url::Url;

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
    pub redelivery: bool,
    pub event: String,
    pub action: Option<String>,
    pub guid: String,
    pub content_type: String,
    pub body: String,
    /// Automatic events obey the active flag and subscription; manual redelivery does not.
    pub automatic: bool,
    /// Native manual sends recheck this requester's current management access before delivery.
    pub manual_actor: Option<i64>,
}

pub(crate) fn json_payload(outgoing: &Outgoing) -> Option<serde_json::Value> {
    if outgoing.content_type == crate::backend::webhooks::settings::FORM_CONTENT_TYPE {
        let json = url::form_urlencoded::parse(outgoing.body.as_bytes())
            .find(|(key, _)| key == "payload")?
            .1
            .into_owned();
        serde_json::from_str(&json).ok()
    } else {
        serde_json::from_str(&outgoing.body).ok()
    }
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
    let payload = hook
        .app_id
        .map(|_| json_payload(outgoing).unwrap_or_default());
    // The hook already received this delivery, so a storage failure retries only the record.
    let mut retries = RETRY_DELAYS.iter();
    loop {
        let stored = match (hook.app_id, &payload) {
            (Some(app), Some(payload)) => {
                codeyard_db_identity::github_app_hooks::insert_delivery(
                    pool,
                    app,
                    &record,
                    outgoing.redelivery,
                    payload["installation"]["id"].as_i64(),
                    payload["repository"]["id"].as_i64(),
                    &hook.payload_url,
                )
                .await
            }
            _ => hooks::insert_delivery(pool, hook.id, &record).await,
        };
        let Err(error) = stored else {
            return;
        };
        tracing::warn!(webhook_id = hook.id, %error, "could not record a webhook delivery");
        match retries.next() {
            Some(delay) => tokio::time::sleep(*delay).await,
            None => return,
        }
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
        redelivery: true,
        event: recorded.event.clone(),
        action: recorded.action.clone(),
        guid: recorded.guid.clone(),
        content_type,
        body: recorded.request_body.clone(),
        automatic: false,
        manual_actor: None,
    }
}

async fn current_hook(
    pool: &DbPool,
    hook: &Webhook,
    outgoing: &Outgoing,
) -> Result<bool, &'static str> {
    if let Some(app) = hook.app_id {
        let mut retries = RETRY_DELAYS.iter();
        loop {
            match crate::backend::webhooks::github_apps::current(pool, app, hook, outgoing).await {
                Ok(current) => return Ok(current),
                Err(error) => {
                    tracing::warn!(webhook_id = hook.id, %error, "could not recheck a queued App webhook");
                    match retries.next() {
                        Some(delay) => tokio::time::sleep(*delay).await,
                        None => return Err("Webhook storage is unavailable"),
                    }
                }
            }
        }
    }
    let owner = match (hook.repo_id, hook.org_id) {
        (Some(id), None) => hooks::HookOwner::Repository(id),
        (None, Some(id)) => hooks::HookOwner::Organization(id),
        _ => return Ok(false),
    };
    let mut retries = RETRY_DELAYS.iter();
    let current = loop {
        let snapshot = match outgoing.manual_actor {
            Some(actor) => hooks::managed_webhook(pool, owner, actor, hook.id).await,
            None => hooks::get_webhook(pool, owner, hook.id).await,
        };
        match snapshot {
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
    // An organization hook gets a repository's events only while the organization owns it. The
    // repository may have been transferred while this send waited.
    if let (Some(org), true) = (hook.org_id, outgoing.automatic) {
        let repository =
            json_payload(outgoing).and_then(|payload| payload["repository"]["id"].as_i64());
        if let Some(repository) = repository {
            let owned =
                codeyard_db_repos::repositories::records::get_repository_by_id(pool, repository)
                    .await
                    .map_err(|_| "Webhook storage is unavailable")?
                    .is_some_and(|repository| repository.owner_org_id == Some(org));
            if !owned {
                return Ok(false);
            }
        }
    }
    Ok(current.payload_url == hook.payload_url
        && current.content_type == hook.content_type
        && current.encrypted_secret == hook.encrypted_secret
        && current.secret_nonce == hook.secret_nonce
        && current.username == hook.username
        && current.encrypted_password == hook.encrypted_password
        && current.password_nonce == hook.password_nonce
        && current.ssl_verify == hook.ssl_verify
        && (!outgoing.automatic
            || (current.active
                && codeyard_model::webhook_events::subscribes(&current.events, &outgoing.event))))
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
    let url = crate::backend::webhooks::outbound::validate_url(
        &hook.payload_url,
        crate::backend::webhooks::outbound::allow_private(),
    )
    .map_err(refusal_text)?;
    let mut headers = vec![
        ("Content-Type", outgoing.content_type.clone()),
        ("User-Agent", user_agent()),
        ("X-GitHub-Event", outgoing.event.clone()),
        ("X-GitHub-Delivery", outgoing.guid.clone()),
        ("X-GitHub-Hook-ID", hook.id.to_string()),
    ];
    if let Some((id, kind)) = hook
        .app_id
        .map(|id| (id, "integration"))
        .or_else(|| hook.org_id.map(|id| (id, "organization")))
        .or_else(|| hook.repo_id.map(|id| (id, "repository")))
    {
        headers.push(("X-GitHub-Hook-Installation-Target-ID", id.to_string()));
        headers.push(("X-GitHub-Hook-Installation-Target-Type", kind.to_owned()));
    }
    if let Some(username) = &hook.username {
        let password = match (&hook.encrypted_password, &hook.password_nonce) {
            (Some(ciphertext), Some(nonce)) => open(ciphertext, nonce)?,
            (None, None) => Vec::new(),
            _ => return Err("The HTTP password could not be opened"),
        };
        let mut credentials = format!("{username}:").into_bytes();
        credentials.extend_from_slice(&password);
        headers.push((
            "Authorization",
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(credentials)
            ),
        ));
    }
    if let Some(secret) = &secret {
        // GitHub also sends the older SHA-1 signature, which some receivers still check.
        let mut legacy =
            Hmac::<sha1::Sha1>::new_from_slice(secret).expect("HMAC accepts any key length");
        legacy.update(outgoing.body.as_bytes());
        headers.push((
            "X-Hub-Signature",
            format!("sha1={}", hex::encode(legacy.finalize().into_bytes())),
        ));
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
        let slot = crate::backend::webhooks::SLOTS
            .acquire()
            .await
            .map_err(|_| "Webhook sends are stopped")?;
        // The hook or its installation may have changed while this send waited for a slot.
        if !current_hook(pool, hook, outgoing).await? {
            return Ok(None);
        }
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
    let request_headers = join_headers(headers.iter().map(|(name, value)| {
        (
            *name,
            if *name == "Authorization" {
                "********"
            } else {
                value.as_str()
            },
        )
    }));
    let duration_ms = i32::try_from(elapsed.as_millis()).unwrap_or(i32::MAX);
    Ok(Some(match attempt {
        Attempt::Responded {
            status,
            headers,
            body,
        } => NewDelivery {
            redelivery: outgoing.redelivery,
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
    let mut response = match bounded_attempt(attempt_inner(url, ssl_verify, headers, body)).await {
        Ok(response) => response,
        Err(failure) => return failure,
    };
    let status = response.status().as_u16();
    let headers = join_headers(
        response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_str().unwrap_or(""))),
    );
    // Response text is diagnostic only: once headers acknowledge the event, a slow body
    // must not turn that acknowledgement into another POST.
    let body = match tokio::time::timeout(Duration::from_secs(2), read_limited(&mut response)).await
    {
        Ok(Ok(bytes)) => response_text(&bytes),
        _ => String::new(),
    };
    Attempt::Responded {
        status,
        headers,
        body,
    }
}

async fn bounded_attempt(
    future: impl std::future::Future<Output = Result<reqwest::Response, Attempt>>,
) -> Result<reqwest::Response, Attempt> {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .unwrap_or(Err(Attempt::Failed {
            error: "The request timed out after 10 seconds",
            retry: true,
        }))
}

async fn attempt_inner(
    url: &Url,
    ssl_verify: bool,
    headers: &[(&str, String)],
    body: &str,
) -> Result<reqwest::Response, Attempt> {
    let addresses = match crate::backend::webhooks::outbound::resolve(
        url,
        crate::backend::webhooks::outbound::allow_private(),
    )
    .await
    {
        Ok(addresses) => addresses,
        Err(refusal) => {
            return Err(Attempt::Failed {
                retry: refusal == Refusal::Unreachable,
                error: refusal_text(refusal),
            })
        }
    };
    let client = match client(url, &addresses, ssl_verify) {
        Ok(client) => client,
        Err(_) => {
            return Err(Attempt::Failed {
                error: "The HTTP client could not start",
                retry: false,
            })
        }
    };
    let mut request = client.post(url.as_str()).body(body.to_owned());
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return Err(Attempt::Failed {
                error: "The request timed out after 10 seconds",
                retry: true,
            })
        }
        Err(error) if error.is_connect() => {
            return Err(Attempt::Failed {
                error: "The connection failed",
                retry: true,
            })
        }
        Err(_) => {
            return Err(Attempt::Failed {
                error: "The request failed",
                retry: false,
            })
        }
    };
    Ok(response)
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
    crate::backend::webhooks::outbound::pinned_client(url, addresses)
        .timeout(TIMEOUT)
        .danger_accept_invalid_certs(!ssl_verify)
        .user_agent(user_agent())
        .build()
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
    open(ciphertext, nonce).map(Some)
}

fn open(ciphertext: &[u8], nonce: &[u8]) -> Result<Vec<u8>, &'static str> {
    let encryptor = SecretEncryptor::configured_from_env()
        .ok()
        .flatten()
        .ok_or("The secret key is not configured")?;
    encryptor
        .decrypt(ciphertext, nonce)
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
        redelivery: outgoing.redelivery,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timeout_covers_the_whole_attempt_and_is_retryable() {
        let result = bounded_attempt(std::future::pending()).await;
        assert!(matches!(result, Err(Attempt::Failed { retry: true, .. })));
    }

    #[test]
    fn binary_response_text_is_database_safe() {
        assert_eq!(response_text(b"ok\0\xff"), "ok\\0\u{fffd}");
    }

    #[test]
    fn preflight_failure_preserves_content_type() {
        let outgoing = Outgoing {
            redelivery: false,
            event: "push".into(),
            action: None,
            guid: "id".into(),
            content_type: "application/x-www-form-urlencoded".into(),
            body: "payload=%7B%7D".into(),
            automatic: true,
            manual_actor: None,
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
            redelivery: false,
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
        assert_eq!(outgoing.guid, "old");
        assert!(outgoing.redelivery);
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
