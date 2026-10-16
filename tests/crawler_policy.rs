//! robots.txt, ai.txt and X-Robots-Tag follow the administrator's AI crawler setting.
//! Block is the default; the administrator can allow crawlers in the admin page or the admin API.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

#[path = "support/database.rs"]
mod database;
#[path = "editor_save/fixture.rs"]
mod fixture;
#[path = "support/native_database.rs"]
mod native;
#[path = "support/production_ci.rs"]
mod production;
#[path = "support/pull_requests.rs"]
mod pulls;

use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use database::Database;
use pulls::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const ALLOW_ALL: &str = "User-agent: *\nDisallow:\n";

async fn fetch(router: &Router, path: &str) -> (StatusCode, HeaderMap, String) {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

fn assert_blocked_robots(robots: &str) {
    for bot in [
        "GPTBot",
        "ChatGPT-User",
        "CCBot",
        "ClaudeBot",
        "anthropic-ai",
        "Google-Extended",
        "PerplexityBot",
        "Bytespider",
        "Applebot-Extended",
        "cohere-ai",
        "Meta-ExternalAgent",
        "Amazonbot",
        "Diffbot",
        "ImagesiftBot",
        "omgili",
        "FacebookBot",
    ] {
        assert!(robots.contains(&format!("User-agent: {bot}\n")), "{bot}");
    }
    // The AI crawler group is disallowed; the group after it stays open.
    assert!(robots.contains("Disallow: /\n\nUser-agent: *\nDisallow:\n"));
}

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.execute(
        "INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE",
        &[],
    )
    .await;
    let admin_id = f.actor("admin").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[admin_id])
        .await;
    let member_id = f.actor("member").await;
    // A fresh install blocks AI crawlers until an administrator allows them.
    let stored = db::node_config::get_node_configuration(f.pool())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.ai_crawlers, "block");
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));

    // Block is the default: AI crawlers are disallowed, and HTML pages carry the header.
    let (status, headers, robots) = fetch(&router, "/robots.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
    assert_blocked_robots(&robots);
    let (_, _, ai) = fetch(&router, "/ai.txt").await;
    assert_eq!(ai, include_str!("../ai.txt"));
    let (_, headers, _) = fetch(&router, "/login").await;
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(headers["x-robots-tag"], "noai, noimageai");
    // Errors on HTML pages carry the header too.
    let (_, headers, _) = fetch(&router, "/no/such/page/here").await;
    assert_eq!(headers["x-robots-tag"], "noai, noimageai");

    // The admin page offers the setting, and the form saves an allow and redirects back.
    let admin = Client::login(&router, f.pool(), admin_id).await;
    let reply = admin
        .form_reply("/admin/ai-crawlers", &[("ai_crawlers", "allow")])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/admin?saved=crawlers"));
    let (_, _, robots) = fetch(&router, "/robots.txt").await;
    assert_eq!(robots, ALLOW_ALL);
    let (_, _, ai) = fetch(&router, "/ai.txt").await;
    assert_eq!(ai, ALLOW_ALL);
    let (_, headers, _) = fetch(&router, "/login").await;
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert!(!headers.contains_key("x-robots-tag"));

    // The admin API reads and writes the same value.
    let (status, value) = admin
        .api("GET", "/api/auth/admin/ai-crawlers", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value, json!({"ai_crawlers": "allow"}));
    let (status, _) = admin
        .api(
            "PUT",
            "/api/auth/admin/ai-crawlers",
            json!({"ai_crawlers": "maybe"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = admin
        .api(
            "PUT",
            "/api/auth/admin/ai-crawlers",
            json!({"ai_crawlers": "block"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, robots) = fetch(&router, "/robots.txt").await;
    assert_blocked_robots(&robots);
    let (_, headers, _) = fetch(&router, "/login").await;
    assert_eq!(headers["x-robots-tag"], "noai, noimageai");

    // Only administrators change the setting; the form refuses a bad value.
    let member = Client::login(&router, f.pool(), member_id).await;
    let (status, _) = member
        .api(
            "PUT",
            "/api/auth/admin/ai-crawlers",
            json!({"ai_crawlers": "block"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let reply = admin
        .form_reply("/admin/ai-crawlers", &[("ai_crawlers", "sometimes")])
        .await;
    // A refused value is not saved or redirected. The page it returns is an error page.
    assert!(!reply.status.is_success() && reply.location.is_none());
    let (_, _, robots) = fetch(&router, "/robots.txt").await;
    assert_blocked_robots(&robots);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_ai_crawler_policy_follows_the_admin_setting() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_ai_crawler_policy_follows_the_admin_setting() {
    contract(true).await;
}
