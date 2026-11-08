//! Browser-approved manifest conversion and signed app webhooks on both real databases.
use super::{github::request, github_apps::jwt};
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body, Bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::post,
    Json, Router,
};

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use codeyard_security::secrets::SecretEncryptor;
use codeyard_storage::ObjectStore;
use rsa::{pkcs8::DecodePrivateKey, RsaPrivateKey};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::{
    net::TcpListener,
    sync::mpsc,
    time::{timeout, Duration},
};
use tower::ServiceExt;
async fn conversion(router: &Router, code: &str) -> (u16, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v3/app-manifests/{code}/conversions"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    assert_github_schema("apps/create-from-manifest", status, &value);
    (status, value)
}
async fn draft(user: &Client, manifest: &Value) -> String {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("manifest", &manifest.to_string())
        .finish();
    let reply = user
        .exchange(
            "POST",
            "/settings/apps/new?state=opaque-state",
            "application/x-www-form-urlencoded",
            body.into_bytes(),
        )
        .await;
    assert_eq!(reply.status, 303, "{}", reply.text());
    let location = reply.location.unwrap();
    let page = user.page_ok(&location).await;
    assert!(page.contains("Approve GitHub App manifest"));
    location.split("manifest=").nth(1).unwrap().to_owned()
}
async fn approve(user: &Client, code: &str) -> String {
    let reply = user
        .form_reply(
            "/settings/apps/new",
            &[("manifest_code", code), ("decision", "approve_manifest")],
        )
        .await;
    assert_eq!(reply.status, 303, "{}", reply.text());
    let url = url::Url::parse(reply.location.as_ref().unwrap()).unwrap();
    assert_eq!(url.host_str(), Some("example.test"));
    let query = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(query.get("state").unwrap().as_ref(), "opaque-state");
    query.get("code").unwrap().to_string()
}
async fn recorded(
    f: &Database,
    app: i64,
    count: usize,
) -> Vec<codeyard_db_identity::github_app_hooks::Delivery> {
    timeout(Duration::from_secs(15), async {
        loop {
            let rows = codeyard_db_identity::github_app_hooks::deliveries(f.pool(), app)
                .await
                .unwrap();
            if rows.len() >= count {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("webhook recorded")
}
async fn contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let repo = codeyard_db_repos::repositories::records::create_repository(
        f.pool(),
        alice,
        "proj",
        None,
        true,
        "alice/proj",
    )
    .await
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(Arc::new(SecretEncryptor::new(&[42; 32]))),
        CiConfig::default(),
        None,
    ));
    let user = Client::login(&router, f.pool(), alice).await;
    let other = Client::login(&router, f.pool(), bob).await;
    let account = tokens::create(
        f.pool(),
        alice,
        Some(&user.session),
        TokenInput {
            name: "test",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let account = format!("Bearer {}", account.value);
    let (tx, mut rx) = mpsc::channel::<(HeaderMap, Vec<u8>)>(10);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let receiver = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route(
                    "/hook",
                    post(
                        |State(tx): State<mpsc::Sender<(HeaderMap, Vec<u8>)>>,
                         headers: HeaderMap,
                         body: Bytes| async move {
                            tx.send((headers, body.to_vec())).await.unwrap();
                            (StatusCode::OK, Json(json!({"accepted":true})))
                        },
                    ),
                )
                .with_state(tx),
        )
        .await
        .unwrap();
    });
    let manifest = json!({"name":"Manifest Bot","url":"https://example.test","redirect_url":"https://example.test/setup","callback_urls":["https://example.test/first","https://example.test/second"],"public":true,"default_permissions":{"issues":"write","contents":"read"},"default_events":["issues"],"hook_attributes":{"url":url}});
    let pending = draft(&user, &manifest).await;
    assert_eq!(conversion(&router, &pending).await.0, 404);
    let invalid = user
        .exchange(
            "POST",
            "/settings/apps/new",
            "application/x-www-form-urlencoded",
            format!("manifest_code={pending}&decision=approve_manifest").into_bytes(),
        )
        .await;
    assert_eq!(invalid.status, 403);
    let code = approve(&user, &pending).await;
    let (a, b) = tokio::join!(conversion(&router, &code), conversion(&router, &code));
    let mut statuses = vec![a.0, b.0];
    statuses.sort_unstable();
    assert_eq!(statuses, vec![201, 404], "{a:?} {b:?}");
    let converted = if a.0 == 201 { a.1 } else { b.1 };
    let id = converted["id"].as_i64().unwrap();
    let pem = converted["pem"].as_str().unwrap();
    let key = RsaPrivateKey::from_pkcs8_pem(pem).unwrap();
    let auth = jwt(
        &key,
        json!(converted["client_id"]),
        chrono::Utc::now().timestamp() + 300,
    );
    let hook = codeyard_db_identity::github_app_hooks::hook(f.pool(), id)
        .await
        .unwrap()
        .unwrap();
    assert!(hook.encrypted_secret.is_some());
    assert_eq!(
        SecretEncryptor::new(&[42; 32])
            .decrypt(
                hook.encrypted_secret.as_ref().unwrap(),
                hook.secret_nonce.as_ref().unwrap()
            )
            .unwrap(),
        converted["webhook_secret"].as_str().unwrap().as_bytes()
    );
    let oauth_page = user
        .page_ok(&format!(
            "/login/oauth/authorize?client_id={}&redirect_uri=https%3A%2F%2Fexample.test%2Fsecond",
            converted["client_id"].as_str().unwrap()
        ))
        .await;
    assert!(oauth_page.contains("https://example.test/second"));
    assert!(oauth_page.contains("repository permissions"));
    let bad = user
        .page(&format!(
            "/login/oauth/authorize?client_id={}&redirect_uri=https%3A%2F%2Fevil.test",
            converted["client_id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(bad.0, 400);
    request(
        &router,
        "apps/get-webhook-config-for-app",
        "GET",
        "/api/v3/app/hook/config",
        Some(&auth),
        Value::Null,
        200,
    )
    .await;
    request(
        &router,
        "apps/get-webhook-config-for-app",
        "GET",
        "/api/v3/app/hook/config",
        Some(&account),
        Value::Null,
        401,
    )
    .await;
    request(
        &router,
        "apps/update-webhook-config-for-app",
        "PATCH",
        "/api/v3/app/hook/config",
        Some(&auth),
        json!({"url":"file:///etc/passwd"}),
        422,
    )
    .await;
    let reply = user
        .form_reply(
            "/apps/manifest-bot/installations/new",
            &[("repository_ids", &repo.id.to_string())],
        )
        .await;
    assert_eq!(reply.status, 303, "{}", reply.text());
    let installation = codeyard_db_identity::github_apps::installations(
        f.pool(),
        &codeyard_db_identity::github_apps::InstallationFilter {
            app: Some(id),
            ..Default::default()
        },
        0,
        1,
    )
    .await
    .unwrap()
    .1[0]
        .id;
    request(
        &router,
        "issues/create",
        "POST",
        "/api/v3/repos/alice/proj/issues",
        Some(&account),
        json!({"title":"Native payload"}),
        201,
    )
    .await;
    let (headers, body) = timeout(Duration::from_secs(15), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(headers["x-github-event"], "issues");
    use hmac::{KeyInit, Mac};
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(
        converted["webhook_secret"].as_str().unwrap().as_bytes(),
    )
    .unwrap();
    mac.update(&body);
    assert_eq!(
        headers["x-hub-signature-256"].to_str().unwrap(),
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    );
    let payload: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload["installation"]["id"], installation);
    assert_eq!(payload["issue"]["title"], "Native payload");
    let rows = recorded(&f, id, 1).await;
    let delivery = rows[0].recorded.id;
    let (_, detail) = request(
        &router,
        "apps/get-webhook-delivery",
        "GET",
        &format!("/api/v3/app/hook/deliveries/{delivery}"),
        Some(&auth),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(detail["request"]["payload"], payload);
    assert_eq!(detail["redelivery"], false);
    assert_eq!(detail["repository_id"], repo.id);
    request(
        &router,
        "apps/redeliver-webhook-delivery",
        "POST",
        &format!("/api/v3/app/hook/deliveries/{delivery}/attempts"),
        Some(&auth),
        Value::Null,
        202,
    )
    .await;
    let (_, resent) = timeout(Duration::from_secs(15), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resent, body);
    let rows = recorded(&f, id, 2).await;
    assert!(rows[0].redelivery);
    assert_eq!(rows[0].recorded.guid, rows[1].recorded.guid);
    let (headers, rows) = request(
        &router,
        "apps/list-webhook-deliveries",
        "GET",
        "/api/v3/app/hook/deliveries?per_page=1&status=success",
        Some(&auth),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    let next = headers["link"]
        .to_str()
        .unwrap()
        .split('<')
        .nth(1)
        .unwrap()
        .split('>')
        .next()
        .unwrap();
    let next = url::Url::parse(next).unwrap();
    let (_, rows) = request(
        &router,
        "apps/list-webhook-deliveries",
        "GET",
        &format!("{}?{}", next.path(), next.query().unwrap()),
        Some(&auth),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(rows[0]["id"], delivery);
    request(
        &router,
        "apps/update-webhook-config-for-app",
        "PATCH",
        "/api/v3/app/hook/config",
        Some(&auth),
        json!({"content_type":"form","secret":""}),
        200,
    )
    .await;
    request(
        &router,
        "issues/create",
        "POST",
        "/api/v3/repos/alice/proj/issues",
        Some(&account),
        json!({"title":"Form payload"}),
        201,
    )
    .await;
    let (headers, body) = timeout(Duration::from_secs(15), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!headers.contains_key("x-hub-signature-256"));
    assert!(body.starts_with(b"payload="));
    recorded(&f, id, 3).await;
    // A redirect never reached the receiver, and a timeout got no answer: both are failures.
    for (guid, status_code, error) in [
        ("redirected", Some(302), None),
        ("timed-out", None, Some("Delivery timed out")),
    ] {
        codeyard_db_identity::github_app_hooks::insert_delivery(
            f.pool(),
            id,
            &codeyard_database::queries::webhooks::NewDelivery {
                redelivery: false,
                guid: guid.into(),
                event: "issues".into(),
                action: Some("opened".into()),
                status_code,
                duration_ms: 1,
                error: error.map(str::to_owned),
                request_headers: String::new(),
                request_body: "{}".into(),
                response_headers: None,
                response_body: None,
            },
            false,
            None,
            Some(repo.id),
            "https://example.test/hook",
        )
        .await
        .unwrap();
    }
    for (status, expected) in [
        ("failure", vec!["timed-out", "redirected"]),
        ("success", Vec::new()),
    ] {
        let (_, rows) = request(
            &router,
            "apps/list-webhook-deliveries",
            "GET",
            &format!("/api/v3/app/hook/deliveries?status={status}"),
            Some(&auth),
            Value::Null,
            200,
        )
        .await;
        let guids: Vec<&str> = rows
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row["guid"].as_str())
            .filter(|guid| matches!(*guid, "redirected" | "timed-out"))
            .collect();
        assert_eq!(guids, expected, "status={status}");
    }
    request(
        &router,
        "apps/remove-repo-from-installation-for-authenticated-user",
        "DELETE",
        &format!(
            "/api/v3/user/installations/{installation}/repositories/{}",
            repo.id
        ),
        Some(&account),
        Value::Null,
        204,
    )
    .await;
    let current = codeyard_db_identity::github_app_hooks::hook(f.pool(), id)
        .await
        .unwrap()
        .unwrap();
    let outgoing =
        codeyard_webhooks::outgoing("issues", Some("opened"), &current, &payload.to_string());
    codeyard_webhooks::delivery::send(f.pool(), &current, &outgoing).await;
    // The three sent deliveries and the two inserted failures; the removed repository adds none.
    assert_eq!(
        codeyard_db_identity::github_app_hooks::deliveries(f.pool(), id)
            .await
            .unwrap()
            .len(),
        5
    );
    assert!(timeout(Duration::from_millis(100), rx.recv())
        .await
        .is_err());
    // A default-private manifest is discoverable/installable only by its owner.
    let pending=draft(&user,&json!({"name":"Private Bot","url":"https://example.test","redirect_url":"https://example.test/setup"})).await;
    let code = approve(&user, &pending).await;
    let (status, private) = conversion(&router, &code).await;
    assert_eq!(status, 201);
    assert_eq!(private["webhook_secret"], Value::Null);
    request(
        &router,
        "apps/get-by-slug",
        "GET",
        "/api/v3/apps/private-bot",
        None,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "apps/get-by-slug",
        "GET",
        "/api/v3/apps/private-bot",
        Some(&account),
        Value::Null,
        200,
    )
    .await;
    let reply = other
        .form_reply(
            "/apps/private-bot/installations/new",
            &[("all_repositories", "yes")],
        )
        .await;
    assert_eq!(reply.status, 403);
    let private_key = RsaPrivateKey::from_pkcs8_pem(private["pem"].as_str().unwrap()).unwrap();
    let private_auth = jwt(
        &private_key,
        private["client_id"].clone(),
        chrono::Utc::now().timestamp() + 300,
    );
    request(
        &router,
        "apps/get-webhook-delivery",
        "GET",
        &format!("/api/v3/app/hook/deliveries/{delivery}"),
        Some(&private_auth),
        Value::Null,
        404,
    )
    .await;
    let reply = user
        .form_reply(
            "/apps/private-bot/installations/new",
            &[("all_repositories", "yes")],
        )
        .await;
    assert_eq!(reply.status, 303);
    let private_id = private["id"].as_i64().unwrap();
    let private_installation = codeyard_db_identity::github_apps::installations(
        f.pool(),
        &codeyard_db_identity::github_apps::InstallationFilter {
            app: Some(private_id),
            ..Default::default()
        },
        0,
        1,
    )
    .await
    .unwrap()
    .1[0]
        .id;
    user.page_ok("/apps/private-bot").await;
    let page = user.page_ok("/settings/apps/installations").await;
    assert!(page.contains("Private Bot"));
    assert!(page.contains("Uninstall GitHub App"));
    let (_, token) = request(
        &router,
        "apps/create-installation-access-token",
        "POST",
        &format!("/api/v3/app/installations/{private_installation}/access_tokens"),
        Some(&private_auth),
        json!({}),
        201,
    )
    .await;
    let token = format!("Bearer {}", token["token"].as_str().unwrap());
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/alice/proj",
        Some(&token),
        Value::Null,
        200,
    )
    .await;
    let reply = other
        .form_reply(
            &format!("/settings/apps/installations/{private_installation}/delete"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 403);
    let reply = user
        .form_reply(
            &format!("/settings/apps/installations/{private_installation}/delete"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 303);
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/alice/proj",
        Some(&token),
        Value::Null,
        401,
    )
    .await;
    let app = codeyard_db_identity::github_apps::app(f.pool(), "manifest-bot")
        .await
        .unwrap()
        .unwrap();
    let password = match f.pool() {
        codeyard_database::DbPool::Sqlite(pool) => {
            sqlx::query("UPDATE users SET password_hash='forbidden' WHERE id=$1")
                .bind(app.bot_id)
                .execute(pool)
                .await
                .map(|_| ())
        }
        codeyard_database::DbPool::Postgres(pool) => {
            sqlx::query("UPDATE users SET password_hash='forbidden' WHERE id=$1")
                .bind(app.bot_id)
                .execute(pool)
                .await
                .map(|_| ())
        }
    };
    assert!(password.is_err());
    assert!(codeyard_db_identity::users::sessions::create_session(
        f.pool(),
        &"c".repeat(64),
        app.bot_id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1)
    )
    .await
    .is_err());
    receiver.abort();
}
#[tokio::test]
async fn github_apps_manifests_and_webhooks_on_both_engines() {
    crate::support::fake_webui_bundle::install();
    std::env::set_var("WEBHOOKS_ALLOW_PRIVATE", "true");
    std::env::set_var("SECRETS_MASTER_KEY", "2a".repeat(32));
    contract(false).await;
    contract(true).await;
}
