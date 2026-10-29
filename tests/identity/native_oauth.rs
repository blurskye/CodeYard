//! Provider sign-in is a browser flow, separate from the token-only API.
use crate::support::{database::Database, fake_webui_bundle, PASSWORD_WORK};
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use codeyard::backend::db;
use tower::ServiceExt;

async fn browser_routes(pg: bool) {
    let _serial = PASSWORD_WORK.lock().await;
    fake_webui_bundle::install();
    let settings = [
        ("PUBLIC_URL", "https://forge.example.test"),
        ("GITLAB_CLIENT_ID", "fixture-client"),
        ("GITLAB_CLIENT_SECRET", "fixture-secret"),
    ];
    let previous: Vec<_> = settings
        .iter()
        .map(|(key, _)| (*key, std::env::var_os(key)))
        .collect();
    for (key, value) in settings {
        std::env::set_var(key, value);
    }
    let mut database = Database::new(pg).await;
    database.complete_setup().await;
    let root = tempfile::tempdir().unwrap();
    let router = database.router(root.path()).await;
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/login/oauth/gitlab?redirect=%2Falice%2Fproj")
                .header(header::HOST, "attacker.invalid")
                .header("x-forwarded-host", "attacker.invalid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    assert!(response.headers()[header::CACHE_CONTROL]
        .to_str()
        .unwrap()
        .contains("no-store"));
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert!(cookie.starts_with("__Host-gitserv_oauth="));
    let location = url::Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    assert_eq!(location.host_str(), Some("gitlab.com"));
    let query: std::collections::HashMap<_, _> = location.query_pairs().into_owned().collect();
    let callback = "https://forge.example.test/login/oauth/gitlab/callback";
    assert_eq!(query["redirect_uri"], callback);
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(!query["code_challenge"].is_empty());
    let state = &query["state"];
    let stored = db::users::sessions::get_oauth_state(database.pool(), state)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.redirect_uri.as_deref(), Some(callback));
    assert_eq!(stored.return_to, "/alice/proj");
    assert_eq!(stored.pkce_verifier.len(), 64);

    for browser in [
        None,
        Some(format!("__Host-gitserv_oauth={}", "a".repeat(64))),
    ] {
        let mut request = Request::builder().uri(format!(
            "/login/oauth/gitlab/callback?code=fixture-code&state={state}"
        ));
        if let Some(browser) = browser {
            request = request.header(header::COOKIE, browser);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("no-store"));
        assert!(db::users::sessions::get_oauth_state(database.pool(), state)
            .await
            .unwrap()
            .is_some());
    }
    for path in [
        "/api/auth/oauth/gitlab".to_owned(),
        format!("/api/auth/oauth/gitlab/callback?code=fixture-code&state={state}"),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().get(header::LOCATION).is_none());
    }
    let response = router
        .oneshot(
            Request::builder()
                .uri("/login/oauth/unknown")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("Unknown OAuth provider."));
    for (key, value) in previous {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
    database.finish().await;
}

#[tokio::test]
async fn sqlite_native_oauth_routes_bind_the_new_callback() {
    browser_routes(false).await;
}

#[tokio::test]
async fn postgres_native_oauth_routes_bind_the_new_callback() {
    browser_routes(true).await;
}
