use super::request;
use crate::support::{
    codeyard_db_orgs::organizations::applied, database::Database, http_client::Client,
};
use axum::{
    http::{HeaderMap, StatusCode},
    routing::post,
    Router,
};
use base64::Engine as _;
use codeyard::backend::api::create_router;
use codeyard::backend::app::AppState;
use codeyard::backend::ci::CiConfig;
use codeyard_support::secrets::SecretEncryptor;

use codeyard_db_orgs::organizations::NewOrganization;
use codeyard_db_orgs::organizations::OrganizationProfile;

use codeyard_db_repos::webhooks::HookOwner;

use codeyard::backend::identity::tokens::TokenInput;
use codeyard::backend::storage::ObjectStore;
use codeyard_db_core::models::OrgRole;
use codeyard_support::crypto::hmac_sha256;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

async fn records(
    f: &Database,
    hook: i64,
    count: usize,
) -> Vec<codeyard_db_core::models::WebhookDelivery> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let rows = codeyard_db_repos::webhooks::list_deliveries(f.pool(), hook)
                .await
                .unwrap();
            if rows.len() >= count {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the queued delivery was recorded")
}
async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let admin = f.actor("delegated").await;
    let outsider = f.actor("outsider").await;
    let org = applied(
        codeyard_db_orgs::organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    );
    for (user, role) in [(member, OrgRole::Member), (admin, OrgRole::Admin)] {
        applied(
            codeyard_db_orgs::organizations::add_organization_member(
                f.pool(),
                owner,
                org.id,
                user,
                role,
            )
            .await
            .unwrap(),
        );
    }
    applied(
        codeyard_db_orgs::organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("other", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let key = Arc::new(SecretEncryptor::new(&[42; 32]));
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().into(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(key.clone()),
        CiConfig::default(),
        None,
    ));
    let mut credentials = Vec::new();
    for (user, scope) in [
        (owner, "write"),
        (owner, "read"),
        (member, "write"),
        (admin, "write"),
        (outsider, "write"),
    ] {
        let client = Client::login(&router, f.pool(), user).await;
        let token = codeyard::backend::identity::tokens::create(
            f.pool(),
            user,
            Some(&client.session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let write = Some(credentials[0].as_str());
    let read = Some(credentials[1].as_str());
    let path = "/api/v3/orgs/acme/hooks";
    let input = json!({"name":"web", "config":{"url":"https://hooks.example/hook", "content_type":"json", "secret":"original-secret", "username":"receiver", "password":"http-password"}, "events":["issues","push"]});
    for token in [
        None,
        Some(credentials[2].as_str()),
        Some(credentials[3].as_str()),
        Some(credentials[4].as_str()),
    ] {
        request(
            &router,
            "orgs/list-webhooks",
            "GET",
            path,
            token,
            Value::Null,
            if token.is_none() { 401 } else { 403 },
        )
        .await;
        request(
            &router,
            "orgs/create-webhook",
            "POST",
            path,
            token,
            input.clone(),
            if token.is_none() { 401 } else { 403 },
        )
        .await;
    }
    request(
        &router,
        "orgs/create-webhook",
        "POST",
        path,
        read,
        input.clone(),
        403,
    )
    .await;
    for input in [
        json!({}),
        json!({"name":"other", "config":{"url":"https://hooks.example"}}),
        json!({"name":"web", "config":{"url":"https://hooks.example", "insecure_ssl":"2"}}),
        json!({"name":"web", "config":{"url":"https://hooks.example", "content_type":"text"}}),
        json!({"name":"web", "config":{"url":"file:///tmp/hook"}}),
    ] {
        request(
            &router,
            "orgs/create-webhook",
            "POST",
            path,
            write,
            input,
            422,
        )
        .await;
    }
    // Refuse private egress unless the operator explicitly permits it.
    std::env::set_var("WEBHOOKS_ALLOW_PRIVATE", "false");
    request(
        &router,
        "orgs/create-webhook",
        "POST",
        path,
        write,
        json!({"name":"web", "config":{"url":"http://127.0.0.1:9/hook"}}),
        422,
    )
    .await;
    std::env::set_var("WEBHOOKS_ALLOW_PRIVATE", "true");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/hook",
                post(move |headers: HeaderMap, body: String| {
                    let tx = tx.clone();
                    async move {
                        tx.send((headers, body)).unwrap();
                        (StatusCode::OK, "ack")
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let mut input = input;
    input["config"]["url"] = json!(url);
    let (headers, body) = request(
        &router,
        "orgs/create-webhook",
        "POST",
        path,
        write,
        input,
        201,
    )
    .await;
    let hook = body["id"].as_i64().unwrap();
    let hook_path = format!("{path}/{hook}");
    assert!(headers["location"].to_str().unwrap().ends_with(&hook_path));
    assert_eq!(body["type"], "Organization");
    assert_eq!(body["config"]["content_type"], "json");
    assert_eq!(body["config"]["secret"], "********");
    assert!(!body.to_string().contains("original-secret"));
    assert!(!body.to_string().contains("http-password"));
    assert_eq!(body["config"]["username"], "receiver");
    assert_eq!(body["config"]["password"], "********");
    let stored =
        codeyard_db_repos::webhooks::get_webhook(f.pool(), HookOwner::Organization(org.id), hook)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        key.decrypt(
            stored.encrypted_secret.as_ref().unwrap(),
            stored.secret_nonce.as_ref().unwrap()
        )
        .unwrap(),
        b"original-secret"
    );
    let (sent_headers, sent_body) = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sent_headers["x-github-event"], "ping");
    assert_eq!(
        sent_headers["authorization"].to_str().unwrap(),
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(b"receiver:http-password")
        )
    );
    assert_eq!(
        key.decrypt(
            stored.encrypted_password.as_ref().unwrap(),
            stored.password_nonce.as_ref().unwrap()
        )
        .unwrap(),
        b"http-password"
    );
    assert_eq!(
        sent_headers["x-hub-signature-256"].to_str().unwrap(),
        format!(
            "sha256={}",
            hex::encode(hmac_sha256(b"original-secret", sent_body.as_bytes()))
        )
    );
    let ping: Value = serde_json::from_str(&sent_body).unwrap();
    assert_eq!(ping["organization"]["login"], "acme");
    assert_eq!(ping["hook"]["config"]["content_type"], "json");
    assert!(!sent_body.contains("original-secret"));
    let rows = records(&f, hook, 1).await;
    let delivery = rows[0].id;
    assert!(!rows[0].redelivery);
    assert!(rows[0].request_headers.contains("Authorization: ********"));
    assert!(!rows[0].request_headers.contains("Basic "));
    let (_, second) = request(
        &router,
        "orgs/create-webhook",
        "POST",
        path,
        write,
        json!({"name":"web", "config":{"url":url}}),
        201,
    )
    .await;
    let second_hook = second["id"].as_i64().unwrap();
    assert_eq!(second["config"]["content_type"], "form");
    assert_eq!(second["active"], true);
    assert_eq!(second["events"], json!(["push"]));
    let (_, body) = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(body.starts_with("payload="));
    records(&f, second_hook, 1).await;
    let (headers, body) = request(
        &router,
        "orgs/list-webhooks",
        "GET",
        &format!("{path}?per_page=1"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], hook);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, body) = request(
        &router,
        "orgs/list-webhooks",
        "GET",
        &format!("{path}?per_page=1&page=2"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["id"], second_hook);
    for (operation, method, suffix) in [
        ("orgs/get-webhook-delivery", "GET", ""),
        ("orgs/redeliver-webhook-delivery", "POST", "/attempts"),
    ] {
        request(
            &router,
            operation,
            method,
            &format!("{path}/{second_hook}/deliveries/{delivery}{suffix}"),
            write,
            Value::Null,
            404,
        )
        .await;
    }
    for (operation, suffix) in [
        ("orgs/get-webhook", ""),
        ("orgs/get-webhook-config-for-org", "/config"),
        ("orgs/list-webhook-deliveries", "/deliveries"),
    ] {
        request(
            &router,
            operation,
            "GET",
            &format!("{hook_path}{suffix}"),
            read,
            Value::Null,
            200,
        )
        .await;
        request(
            &router,
            operation,
            "GET",
            &format!("/api/v3/orgs/other/hooks/{hook}{suffix}"),
            write,
            Value::Null,
            404,
        )
        .await;
        for token in credentials.iter().skip(2) {
            request(
                &router,
                operation,
                "GET",
                &format!("{hook_path}{suffix}"),
                Some(token),
                Value::Null,
                403,
            )
            .await;
        }
    }
    let delivery_path = format!("{hook_path}/deliveries/{delivery}");
    let (_, body) = request(
        &router,
        "orgs/get-webhook-delivery",
        "GET",
        &delivery_path,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["request"]["payload"]["hook_id"], hook);
    assert_eq!(body["response"]["payload"], "ack");
    assert_eq!(body["status_code"], 200);
    assert_eq!(body["status"], "OK");
    assert_eq!(body["request"]["headers"]["X-GitHub-Event"], "ping");
    for (operation, method, suffix, input) in [
        (
            "orgs/update-webhook",
            "PATCH",
            "".to_owned(),
            json!({"active":false}),
        ),
        (
            "orgs/update-webhook-config-for-org",
            "PATCH",
            "/config".into(),
            json!({"secret":"rotated-secret"}),
        ),
        ("orgs/ping-webhook", "POST", "/pings".into(), Value::Null),
        (
            "orgs/redeliver-webhook-delivery",
            "POST",
            format!("/deliveries/{delivery}/attempts"),
            Value::Null,
        ),
        ("orgs/delete-webhook", "DELETE", "".into(), Value::Null),
    ] {
        for token in credentials.iter().skip(1) {
            request(
                &router,
                operation,
                method,
                &format!("{hook_path}{suffix}"),
                Some(token),
                input.clone(),
                403,
            )
            .await;
        }
        request(
            &router,
            operation,
            method,
            &format!("/api/v3/orgs/other/hooks/{hook}{suffix}"),
            write,
            input,
            404,
        )
        .await;
    }
    let (_, body) = request(
        &router,
        "orgs/update-webhook",
        "PATCH",
        &hook_path,
        write,
        json!({"active":false}),
        200,
    )
    .await;
    assert_eq!(body["active"], false);
    assert_eq!(body["events"], json!(["issues", "push"]));
    assert_eq!(body["config"]["url"], url);
    // A full update without `config` keeps the stored secret.
    let stored =
        codeyard_db_repos::webhooks::get_webhook(f.pool(), HookOwner::Organization(org.id), hook)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        key.decrypt(
            stored.encrypted_secret.as_ref().unwrap(),
            stored.secret_nonce.as_ref().unwrap()
        )
        .unwrap(),
        b"original-secret"
    );
    let (_, body) = request(
        &router,
        "orgs/update-webhook-config-for-org",
        "PATCH",
        &format!("{hook_path}/config"),
        write,
        json!({"secret":"rotated-secret", "password":"rotated-password"}),
        200,
    )
    .await;
    assert_eq!(body["url"], url);
    assert_eq!(body["secret"], "********");
    let stored =
        codeyard_db_repos::webhooks::get_webhook(f.pool(), HookOwner::Organization(org.id), hook)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        key.decrypt(
            stored.encrypted_secret.as_ref().unwrap(),
            stored.secret_nonce.as_ref().unwrap()
        )
        .unwrap(),
        b"rotated-secret"
    );
    request(
        &router,
        "orgs/redeliver-webhook-delivery",
        "POST",
        &format!("{delivery_path}/attempts"),
        write,
        Value::Null,
        202,
    )
    .await;
    let (resent_headers, resent_body) = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resent_body, sent_body);
    assert_eq!(
        resent_headers["authorization"].to_str().unwrap(),
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(b"receiver:rotated-password")
        )
    );
    assert_eq!(
        resent_headers["x-github-delivery"],
        sent_headers["x-github-delivery"]
    );
    assert_eq!(
        resent_headers["x-hub-signature-256"].to_str().unwrap(),
        format!(
            "sha256={}",
            hex::encode(hmac_sha256(b"rotated-secret", resent_body.as_bytes()))
        )
    );
    let rows = records(&f, hook, 2).await;
    assert!(rows[0].redelivery);
    let (headers, body) = request(
        &router,
        "orgs/list-webhook-deliveries",
        "GET",
        &format!("{hook_path}/deliveries?per_page=1&status=success"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], rows[0].id);
    let next = headers["link"]
        .to_str()
        .unwrap()
        .split('>')
        .next()
        .unwrap()
        .trim_start_matches('<');
    let next = url::Url::parse(next).unwrap();
    let (_, body) = request(
        &router,
        "orgs/list-webhook-deliveries",
        "GET",
        &format!("{}?{}", next.path(), next.query().unwrap()),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["id"], delivery);
    let (_, body) = request(
        &router,
        "orgs/list-webhook-deliveries",
        "GET",
        &format!("{hook_path}/deliveries?status=failure"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body, json!([]));
    // The native form can also remove a secret; no plaintext enters settings HTML.
    let browser = Client::login(&router, f.pool(), owner).await;
    let page = browser
        .page_ok(&format!(
            "/settings/organizations/{}?tab=webhooks&hook={hook}&edit=1",
            org.id
        ))
        .await;
    assert!(!page.contains("rotated-secret"));
    assert!(!page.contains("rotated-password"));
    assert!(page.contains("clear_password"));
    assert!(page.contains("clear_secret"));
    let hook_id = hook.to_string();
    // The edit form carries the revision it was rendered from.
    let revision = crate::support::http_client::hidden_value(&page, "expected_updated");
    let (status, page) = browser
        .form(
            &format!("/settings/organizations/{}/webhooks/webhook-save", org.id),
            &[
                ("hook_id", &hook_id),
                ("expected_updated", &revision),
                ("payload_url", &url),
                ("content_type", "application/json"),
                ("events", "issues"),
                ("ssl_verify", "yes"),
                ("clear_secret", "yes"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let stored =
        codeyard_db_repos::webhooks::get_webhook(f.pool(), HookOwner::Organization(org.id), hook)
            .await
            .unwrap()
            .unwrap();
    assert!(stored.encrypted_secret.is_none());
    assert!(stored.secret_nonce.is_none());
    assert_eq!(
        key.decrypt(
            stored.encrypted_password.as_deref().unwrap(),
            stored.password_nonce.as_deref().unwrap()
        )
        .unwrap(),
        b"rotated-password"
    );
    let page = browser
        .page_ok(&format!(
            "/settings/organizations/{}?tab=webhooks&hook={hook}&edit=1",
            org.id
        ))
        .await;
    let revision = crate::support::http_client::hidden_value(&page, "expected_updated");
    let (status, page) = browser
        .form(
            &format!("/settings/organizations/{}/webhooks/webhook-save", org.id),
            &[
                ("hook_id", &hook_id),
                ("expected_updated", &revision),
                ("payload_url", &url),
                ("content_type", "application/json"),
                ("events", "issues"),
                ("ssl_verify", "yes"),
                ("clear_password", "yes"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let stored =
        codeyard_db_repos::webhooks::get_webhook(f.pool(), HookOwner::Organization(org.id), hook)
            .await
            .unwrap()
            .unwrap();
    assert!(stored.username.is_none());
    assert!(stored.encrypted_password.is_none());
    assert!(stored.password_nonce.is_none());
    request(
        &router,
        "orgs/ping-webhook",
        "POST",
        &format!("{hook_path}/pings"),
        write,
        Value::Null,
        204,
    )
    .await;
    let (headers, _) = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!headers.contains_key("x-hub-signature-256"));
    assert!(!headers.contains_key("authorization"));
    records(&f, hook, 3).await;
    // Only supplied fields change. Explicit empty secrets clear both encrypted columns.
    request(
        &router,
        "orgs/update-webhook-config-for-org",
        "PATCH",
        &format!("{hook_path}/config"),
        write,
        json!({"secret":"temporary"}),
        200,
    )
    .await;
    request(
        &router,
        "orgs/update-webhook-config-for-org",
        "PATCH",
        &format!("{hook_path}/config"),
        write,
        json!({"secret":""}),
        200,
    )
    .await;
    let stored =
        codeyard_db_repos::webhooks::get_webhook(f.pool(), HookOwner::Organization(org.id), hook)
            .await
            .unwrap()
            .unwrap();
    assert!(stored.encrypted_secret.is_none());
    assert!(stored.secret_nonce.is_none());
    // A full update with `config` replaces the configuration, so an omitted secret is cleared.
    request(
        &router,
        "orgs/update-webhook-config-for-org",
        "PATCH",
        &format!("{hook_path}/config"),
        write,
        json!({"secret":"temporary"}),
        200,
    )
    .await;
    request(
        &router,
        "orgs/update-webhook",
        "PATCH",
        &hook_path,
        write,
        json!({"config":{"url":url}}),
        200,
    )
    .await;
    let stored =
        codeyard_db_repos::webhooks::get_webhook(f.pool(), HookOwner::Organization(org.id), hook)
            .await
            .unwrap()
            .unwrap();
    assert!(stored.encrypted_secret.is_none());
    request(
        &router,
        "orgs/get-webhook-delivery",
        "GET",
        &format!("{hook_path}/deliveries/999999"),
        read,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/redeliver-webhook-delivery",
        "POST",
        &format!("{hook_path}/deliveries/999999/attempts"),
        write,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/delete-webhook",
        "DELETE",
        &hook_path,
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/get-webhook",
        "GET",
        &hook_path,
        read,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/delete-webhook",
        "DELETE",
        &hook_path,
        write,
        Value::Null,
        404,
    )
    .await;
    assert!(codeyard_db_repos::webhooks::list_deliveries(f.pool(), hook)
        .await
        .unwrap()
        .is_empty());
    request(
        &router,
        "orgs/delete-webhook",
        "DELETE",
        &format!("{path}/{second_hook}"),
        write,
        Value::Null,
        204,
    )
    .await;
    server.abort();
    f.finish().await;
}

// A single test owns the process-wide outbound policy and signing key on both engines.
#[tokio::test]
async fn github_organization_hooks_sqlite_and_postgres() {
    crate::support::fake_webui_bundle::install();
    std::env::set_var("SECRETS_MASTER_KEY", "2a".repeat(32));
    contract(false).await;
    contract(true).await;
}
