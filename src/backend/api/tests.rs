//! Router-level tests: the composed app's health, public config, asset, error and form
//! redirect behaviour, checked through `create_router` without a database.
use super::*;
use crate::backend::db;
use axum::body::{to_bytes, Body};
use axum::http::StatusCode;
use std::path::PathBuf;
use tower::ServiceExt;

pub(super) async fn closed_state() -> AppState {
    // A closed lazy pool can't reach any server or change any data.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgresql://fixture:fixture@127.0.0.1:1/ssr_fixture")
        .unwrap();
    pool.close().await;
    let object_root = tempfile::tempdir().unwrap();
    let storage = Arc::new(
        crate::backend::storage::ObjectStore::open(object_root.path(), None)
            .await
            .unwrap(),
    );
    AppState::new(
        db::DbPool::from(pool),
        PathBuf::from("/unused-ssr-fixture"),
        storage,
        None,
        crate::backend::ci::CiConfig::default(),
        None,
    )
}

#[tokio::test]
async fn incomplete_native_forms_are_bounded_and_expire_before_extraction() {
    use futures_util::StreamExt;
    use std::time::Duration;

    let state = closed_state().await;
    let capacity = state.form_reads.clone();
    let app = create_router(state);
    let (started, mut reads) = tokio::sync::mpsc::unbounded_channel();
    let mut requests = Vec::new();
    for index in 0..8 {
        let started = started.clone();
        // Half declare a lookalike form type and one sends JSON. Every mutating body
        // takes a slot, whatever its Content-Type says.
        let content_type = match index {
            0..=3 => "application/x-www-form-urlencodedx",
            4 => "application/json",
            _ => "application/x-www-form-urlencoded",
        };
        let body = futures_util::stream::once(async move {
            started.send(()).unwrap();
            Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'x'; 4 * 1024 * 1024 - 1]))
        })
        .chain(futures_util::stream::pending());
        requests.push(tokio::spawn(
            app.clone().oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/ghost/repo/_forms/write-file")
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::CONTENT_LENGTH, "4194304")
                    .body(Body::from_stream(body))
                    .unwrap(),
            ),
        ));
    }
    for _ in 0..8 {
        tokio::time::timeout(Duration::from_secs(3), reads.recv())
            .await
            .unwrap()
            .unwrap();
    }
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        app.clone().oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/ghost/repo/_forms/write-file")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from_stream(futures_util::stream::pending::<
                    Result<bytes::Bytes, std::io::Error>,
                >()))
                .unwrap(),
        ),
    )
    .await
    .expect("busy forms must not read or queue")
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Git is not subject to form admission, even with a misleading content type.
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        app.clone().oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/ghost/repo.git/git-upload-pack")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    tokio::time::timeout(Duration::from_secs(32), async {
        for request in requests {
            assert_eq!(
                request.await.unwrap().unwrap().status(),
                StatusCode::REQUEST_TIMEOUT
            );
        }
    })
    .await
    .expect("incomplete forms must expire");
    assert_eq!(capacity.available_permits(), 8);
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/ghost/repo/_forms/write-file")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(vec![b'x'; 4 * 1024 * 1024 + 1]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn health_readiness_and_public_config_do_not_require_setup_or_a_session() {
    let app = create_router(closed_state().await);
    for (path, expected) in [
        ("/health", StatusCode::OK),
        ("/ready", StatusCode::SERVICE_UNAVAILABLE),
        ("/api/config", StatusCode::OK),
    ] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if path == "/api/config" {
            assert!(json["http_clone_base"].is_string());
            assert!(json["ssh_clone_base"].is_string());
            assert!(json["ssh_mode"].is_string());
            assert!(json["ssh_setup_required"].is_boolean());
            assert_eq!(json.as_object().unwrap().len(), 4);
        }
    }
}

#[tokio::test]
async fn all_real_lane_routers_construct_and_infrastructure_failure_is_html_503() {
    // Constructing the complete router catches Axum route/wildcard conflicts.
    let app = create_router(closed_state().await);
    for path in [
        "/assets/favicon.png",
        "/assets/logo.webp",
        "/assets/logo.svg",
    ] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/assets/missing.css")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    for path in [
        "/",
        "/setup",
        "/login",
        "/missing/unknown/page",
        "/alice/repo/raw/main/README.md",
        "/alice/repo/archive/feature%2Ftest.zip",
    ] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert!(response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("script-src 'self' 'wasm-unsafe-eval'"));
        let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("fixture:fixture"));
        assert!(!html.contains("PoolClosed"));
    }
}

#[tokio::test]
async fn a_get_at_a_form_url_goes_to_the_page_the_form_belongs_to() {
    let app = create_router(closed_state().await);
    for (uri, location) in [
        ("/alice/demo/_forms/settings", "/alice/demo/settings"),
        ("/alice/demo/_forms/delete-label", "/alice/demo/labels"),
        ("/alice/demo/_forms/create-branch", "/alice/demo/branches"),
        ("/alice/demo/_forms/delete-tag", "/alice/demo/tags"),
        ("/alice/demo/_forms/star", "/alice/demo"),
        // The editor form names its own page.
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2Fmain%2Fdocs%2Fa%2520b.md",
            "/alice/demo/edit/main/docs/a%20b.md",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fnew%2Fmain",
            "/alice/demo/new/main",
        ),
        // Another repository's page, or anything else, is not followed.
        (
            "/alice/demo/_forms/write-file?from=%2Fbob%2Fother%2Fedit%2Fmain%2Fa.md",
            "/alice/demo",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2F%2Fevil.example",
            "/alice/demo",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2F..%2F..%2F..%2Fbob%2Fother",
            "/alice/demo",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2F%2e%2e%2F%2e%2e%2Fbob%2Fother",
            "/alice/demo",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2F%2E%2E%2Fbob",
            "/alice/demo",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2F.%2e%2Fbob",
            "/alice/demo",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2F%2e.%2Fbob",
            "/alice/demo",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2F%252e%252e%2Fbob",
            "/alice/demo",
        ),
        // The editor URL of a file literally named `%2e` is not a dot segment.
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2Fmain%2F%25252e",
            "/alice/demo/edit/main/%252e",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2Fmain%2F.%2Fa.md",
            "/alice/demo",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2F%2Fevil.example",
            "/alice/demo",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2Fmain%5C..%5Cx",
            "/alice/demo",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2Fmain%0D%0Ax",
            "/alice/demo",
        ),
        (
            "/alice/demo/_forms/write-file?from=%2Falice%2Fdemo%2Fedit%2Fmain%2F%5Cevil",
            "/alice/demo",
        ),
        ("/alice/demo/_forms/write-file", "/alice/demo"),
        ("/settings/profile", "/settings?tab=profile"),
        ("/settings/password", "/settings?tab=password"),
        ("/settings/keys", "/settings?tab=keys"),
        ("/settings/keys/7/delete", "/settings?tab=keys"),
        ("/settings/tokens", "/settings?tab=tokens"),
        ("/settings/tokens/7/delete", "/settings?tab=tokens"),
        ("/settings/appearance", "/settings?tab=appearance"),
        ("/alice/demo/commits/main", "/alice/demo/commits?ref=main"),
        (
            "/alice/demo/commits/feature/x%20y",
            "/alice/demo/commits?ref=feature%2Fx+y",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(response.headers()[header::LOCATION], location, "{uri}");
    }
}
