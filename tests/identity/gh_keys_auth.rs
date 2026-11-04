//! Requests emitted by gh 2.102.0 for SSH keys and Enterprise auth status.
use crate::support::{database::Database, http_client::Client};
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request},
    Router,
};
use codeyard_identity::tokens::{self, TokenInput};
use serde_json::{json, Value};
use tower::ServiceExt;

const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
const USER_CURRENT: &str = "query UserCurrent{viewer{login}}";

pub(super) async fn send(
    router: &Router,
    method: &str,
    path: &str,
    authorization: &str,
    body: Option<Value>,
    expected: u16,
) -> (HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", authorization);
    let body = match body {
        Some(body) => {
            request = request.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status.as_u16(),
        expected,
        "{method} {path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (headers, value)
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("gh-alice").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await;
    let mut credentials = Vec::new();
    for scope in ["write", "read"] {
        let issued = tokens::create(
            f.pool(),
            alice,
            Some(&session.session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push((issued.token.id, format!("token {}", issued.value)));
    }
    let write = &credentials[0].1;
    let read = &credentials[1].1;

    // Environment tokens need UserCurrent; stored accounts only need the REST scope probe.
    for credential in [write, read] {
        let (_, body) = send(
            &router,
            "POST",
            "/api/graphql",
            credential,
            Some(json!({"query":USER_CURRENT})),
            200,
        )
        .await;
        assert!(body.get("errors").is_none(), "{body}");
        assert_eq!(body["data"]["viewer"]["login"], "gh-alice");
        let (headers, _) = send(&router, "GET", "/api/v3/", credential, None, 200).await;
        let scopes = headers["x-oauth-scopes"]
            .to_str()
            .unwrap()
            .split(',')
            .map(str::trim)
            .collect::<Vec<_>>();
        assert!(scopes.contains(&"repo"));
        assert!(scopes.contains(&"read:org"));
    }

    for (path, default_title) in [
        ("/api/v3/user/keys", "SSH key"),
        ("/api/v3/user/ssh_signing_keys", "SSH signing key"),
    ] {
        let list = format!("{path}?per_page=100");
        let (_, keys) = send(&router, "GET", &list, write, None, 200).await;
        assert_eq!(keys, json!([]));
        let input = json!({"key":format!("{KEY} laptop@example.com\n"),"title":""});
        send(&router, "POST", path, read, Some(input.clone()), 403).await;
        send(
            &router,
            "POST",
            path,
            write,
            Some(json!({"key":"ssh-ed25519 AAAA","title":""})),
            422,
        )
        .await;
        send(&router, "POST", path, write, Some(input.clone()), 201).await;
        let (_, keys) = send(&router, "GET", &list, read, None, 200).await;
        let keys = keys.as_array().unwrap();
        assert_eq!(keys.len(), 1);
        let key = &keys[0];
        assert!(key["id"].as_i64().unwrap() > 0);
        assert_eq!(key["title"], default_title);
        assert_eq!(
            key["key"], KEY,
            "gh's duplicate-upload comparison must ignore the comment"
        );
        chrono::DateTime::parse_from_rfc3339(key["created_at"].as_str().unwrap()).unwrap();
        send(&router, "POST", path, write, Some(input), 422).await;
        if path.ends_with("/keys") {
            let item = format!("{path}/{}", key["id"]);
            let (_, fetched) = send(&router, "GET", &item, write, None, 200).await;
            assert_eq!(fetched["title"], default_title);
            send(&router, "DELETE", &item, read, None, 403).await;
            send(&router, "DELETE", &item, write, None, 204).await;
            send(&router, "GET", &item, write, None, 404).await;
            let (_, empty) = send(&router, "GET", &list, write, None, 200).await;
            assert_eq!(empty, json!([]));
            send(
                &router,
                "POST",
                path,
                write,
                Some(json!({"key":KEY,"title":"Work laptop"})),
                201,
            )
            .await;
            let (_, named) = send(&router, "GET", &list, write, None, 200).await;
            assert_eq!(named[0]["title"], "Work laptop");
        }
    }

    // Bad and revoked credentials must not look like healthy configured accounts.
    send(&router, "GET", "/api/v3/", "token invalid", None, 401).await;
    f.execute(
        "DELETE FROM personal_access_tokens WHERE id=$1",
        &[credentials[0].0],
    )
    .await;
    send(&router, "GET", "/api/v3/", write, None, 401).await;
    let (_, body) = send(
        &router,
        "POST",
        "/api/graphql",
        write,
        Some(json!({"query":USER_CURRENT})),
        401,
    )
    .await;
    assert_eq!(body["message"], "Bad credentials");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_gh_keys_auth() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_gh_keys_auth() {
    contract(true).await;
}
