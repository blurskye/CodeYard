//! Browser auth contracts: CSRF synchronizers, native auth forms, setup names and
//! temporary-password settings. Page navigation admission is tested elsewhere.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
#[allow(dead_code)]
use crate::support::fake_webui_bundle;
use crate::support::native_database as native;

use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderMap, Request, StatusCode},
    Router,
};
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db,
    db::users::transitions::{self, SetupClaim},
    identity::{
        self,
        admission::AdmissionError,
        credentials::PasswordAccount,
        passwords,
        sessions::{InitiatingSession, SessionContext},
        setup::{SetupError, SetupInput},
    },
    storage::ObjectStore,
};
use database::Database;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

/// Password hashing has four process-wide permits, and provider settings are
/// environment variables. The contracts in this file therefore run one at a time.
use crate::support::PASSWORD_WORK as SERIAL;
const SSH_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

async fn session_with_csrf(pool: &db::DbPool, user: Option<i64>) -> (String, String) {
    let session = native::token();
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(1);
    if let Some(user) = user {
        db::users::sessions::create_session(pool, &session, user, None, None, expires_at)
            .await
            .unwrap();
    } else {
        db::users::sessions::create_browser_session(pool, &session, None, None, expires_at)
            .await
            .unwrap();
    }
    let csrf = native::token();
    assert_eq!(
        db::users::sessions::get_or_create_csrf_token(pool, &session, user, &csrf, 3600)
            .await
            .unwrap(),
        Some(csrf.clone())
    );
    (session, csrf)
}

async fn post(
    router: &Router,
    path: &str,
    session: &str,
    csrf: Option<&str>,
    origin: &str,
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, "forge.example.test")
        .header(header::ORIGIN, origin)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("session_id={session}"));
    if let Some(csrf) = csrf {
        request = request.header("x-csrf-token", csrf);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, body)
}

async fn contract(pg: bool) {
    let _serial = SERIAL.lock().await;
    let mut fixture = Database::new(pg).await;
    // The shared PostgreSQL fixture normally provisions a CI owner and repository.
    // This contract needs the real first-run state instead.
    fixture.execute("DELETE FROM repositories", &[]).await;
    fixture.execute("DELETE FROM users", &[]).await;
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        fixture.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));

    // The setup page owns an anonymous synchronizer before node admission exists.
    let (setup_session, setup_csrf) = session_with_csrf(fixture.pool(), None).await;
    let setup = json!({
        "username": "root_admin",
        "email": "root_admin@example.test",
        "password": "setup-password",
        "node_mode": "private",
        "admin_secret": std::env::var("ADMIN_SECRET").ok(),
    });
    let (status, _, body) = post(
        &router,
        "/api/auth/setup",
        &setup_session,
        Some(&setup_csrf),
        "http://forge.example.test",
        setup,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "setup: {body}");

    let password_hash = codeyard::backend::identity::passwords::hash_password("current-password")
        .await
        .unwrap();
    let login_user = db::users::accounts::create_user_with_password_and_flags(
        fixture.pool(),
        "private-user",
        None,
        &password_hash,
        false,
        false,
    )
    .await
    .unwrap();

    // A private node may admit the login endpoint even though ordinary pages redirect.
    let (login_session, login_csrf) = session_with_csrf(fixture.pool(), None).await;
    let credentials = json!({"username": login_user.username, "password": "current-password"});
    let (status, _, body) = post(
        &router,
        "/api/auth/login",
        &login_session,
        Some(&login_csrf),
        "https://attacker.invalid",
        credentials.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "cross-origin login: {body}");
    assert_eq!(body["error"], "csrf_error");
    let (status, _, body) = post(
        &router,
        "/api/auth/login",
        &login_session,
        Some(&login_csrf),
        "http://forge.example.test",
        credentials,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "private login: {body}");

    // A temporary-password session may submit only the operation that removes its restriction.
    let forced_user = db::users::accounts::create_user_with_password_and_flags(
        fixture.pool(),
        "forced-user",
        None,
        &password_hash,
        false,
        true,
    )
    .await
    .unwrap();
    let (forced_session, forced_csrf) =
        session_with_csrf(fixture.pool(), Some(forced_user.id)).await;
    let password_change =
        json!({"current_password": "current-password", "new_password": "replacement-password"});
    let (status, _, body) = post(
        &router,
        "/api/auth/change-password",
        &forced_session,
        None,
        "http://forge.example.test",
        password_change.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "missing token: {body}");
    assert_eq!(body["error"], "csrf_error");
    let (status, headers, body) = post(
        &router,
        "/api/auth/change-password",
        &forced_session,
        Some(&forced_csrf),
        "http://forge.example.test",
        password_change,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "required password change: {body}");
    // The session is gone, so the client is told to drop its cookie.
    let cookie = headers[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.contains("Max-Age=0"), "{cookie}");

    let admin = db::users::accounts::get_user_by_username(fixture.pool(), "root_admin")
        .await
        .unwrap()
        .unwrap();
    let (logout_session, logout_csrf) = session_with_csrf(fixture.pool(), Some(admin.id)).await;
    fixture
        .execute(
            "UPDATE sessions SET csrf_expires_at='2000-01-01T00:00:00Z' WHERE user_id=$1",
            &[admin.id],
        )
        .await;
    let (status, _, body) = post(
        &router,
        "/api/auth/logout",
        &logout_session,
        Some(&logout_csrf),
        "http://forge.example.test",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "expired token: {body}");

    let candidate = native::token();
    let fresh_csrf = db::users::sessions::get_or_create_csrf_token(
        fixture.pool(),
        &logout_session,
        Some(admin.id),
        &candidate,
        3600,
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(fresh_csrf, logout_csrf);
    let (status, headers, body) = post(
        &router,
        "/api/auth/logout",
        &logout_session,
        Some(&fresh_csrf),
        "http://forge.example.test",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "logout: {body}");
    assert!(headers.get(header::SET_COOKIE).is_some());
    assert!(
        db::users::sessions::get_session_account(fixture.pool(), &logout_session)
            .await
            .unwrap()
            .is_none()
    );

    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_auth_csrf_uses_session_admission() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_auth_csrf_uses_session_admission() {
    contract(true).await;
}

async fn get(
    router: &Router,
    path: &str,
    session: Option<&str>,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder()
        .method("GET")
        .uri(path)
        .header(header::HOST, "forge.example.test")
        .header(header::ACCEPT, "text/html");
    if let Some(session) = session {
        request = request.header(header::COOKIE, format!("session_id={session}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// Submits a native HTML form the way a browser does, with same-origin headers.
async fn native_form(
    router: &Router,
    path: &str,
    session: &str,
    fields: &[(&str, &str)],
) -> (StatusCode, String) {
    let mut body = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        body.append_pair(name, value);
    }
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, "forge.example.test")
        .header(header::ORIGIN, "http://forge.example.test")
        .header(header::ACCEPT, "text/html")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, format!("session_id={session}"))
        .body(Body::from(body.finish()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn test_router(fixture: &Database, root: &std::path::Path) -> Router {
    create_router(AppState::new(
        fixture.pool().clone(),
        root.to_owned(),
        Arc::new(ObjectStore::open(root, None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ))
}

async fn temporary_password_settings_contract(pg: bool) {
    let _serial = SERIAL.lock().await;
    fake_webui_bundle::install();
    let mut fixture = Database::new(pg).await;
    fixture.execute("DELETE FROM repositories", &[]).await;
    fixture.execute("DELETE FROM users", &[]).await;
    let hash = passwords::hash_password("current-password").await.unwrap();
    // A private node gives the administrator an access tab with a pending applicant.
    assert!(matches!(
        transitions::complete_setup(fixture.pool(), "root_admin", None, &hash, "private", None)
            .await
            .unwrap(),
        SetupClaim::Completed(_)
    ));
    let owner = db::users::accounts::create_user_with_password_and_flags(
        fixture.pool(),
        "owner_admin",
        None,
        &hash,
        true,
        false,
    )
    .await
    .unwrap();
    db::users::ssh_keys::add_ssh_key(fixture.pool(), owner.id, "owner-key-marker", SSH_KEY)
        .await
        .unwrap();
    let session = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    db::users::sessions::create_session(
        fixture.pool(),
        session,
        owner.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    db::users::tokens::create_token(
        fixture.pool(),
        owner.id,
        session,
        "owner-token-marker",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "read",
        None,
    )
    .await
    .unwrap();
    db::users::signup_requests::create_signup_request(
        fixture.pool(),
        "applicant_marker",
        Some("applicant-marker@example.test"),
        &hash,
    )
    .await
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let router = test_router(&fixture, root.path()).await;
    let (session, _) = session_with_csrf(fixture.pool(), Some(owner.id)).await;

    // Control: before the password is temporary, every tab shows its records.
    for (path, marker) in [
        ("/settings?tab=keys", "owner-key-marker"),
        ("/settings?tab=tokens", "owner-token-marker"),
        ("/settings?tab=access", "applicant-marker@example.test"),
    ] {
        let (status, _, body) = get(&router, path, Some(&session)).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert!(body.contains(marker), "{path} control: {body}");
    }

    fixture
        .execute(
            "UPDATE users SET must_change_password=TRUE WHERE id=$1",
            &[owner.id],
        )
        .await;
    for path in [
        "/settings",
        "/settings?tab=profile",
        "/settings?tab=keys",
        "/settings?tab=tokens",
        "/settings?tab=access",
    ] {
        let (status, headers, body) = get(&router, path, Some(&session)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{path}: {body}");
        assert_eq!(
            headers[header::LOCATION].to_str().unwrap(),
            "/settings?tab=password",
            "{path}"
        );
        for marker in [
            "owner-key-marker",
            "owner-token-marker",
            "applicant_marker",
            "applicant-marker@example.test",
        ] {
            assert!(!body.contains(marker), "{path} leaked {marker}");
        }
    }
    let (status, _, body) = get(&router, "/settings?tab=password", Some(&session)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("action=\"/settings/password\""), "{body}");
    fixture.finish().await;
}

async fn setup_name_contract(pg: bool) {
    let _serial = SERIAL.lock().await;
    let mut fixture = Database::new(pg).await;
    fixture.execute("DELETE FROM repositories", &[]).await;
    fixture.execute("DELETE FROM users", &[]).await;
    let root = tempfile::tempdir().unwrap();
    let router = test_router(&fixture, root.path()).await;
    let (session, csrf) = session_with_csrf(fixture.pool(), None).await;
    let setup = |username: &str| {
        json!({
            "username": username,
            "password": "setup-password",
            "node_mode": "private",
            "admin_secret": std::env::var("ADMIN_SECRET").ok(),
        })
    };

    // Creating a new account named `admin` is refused, and setup stays incomplete.
    let (status, _, body) = post(
        &router,
        "/api/auth/setup",
        &session,
        Some(&csrf),
        "http://forge.example.test",
        setup("admin"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "validation_error");
    assert_eq!(body["message"], "That username is reserved");
    assert!(!db::node_config::get_node_configuration(fixture.pool())
        .await
        .unwrap()
        .is_some_and(|config| config.setup_completed));

    // An existing administrator named `admin` keeps that name and can finish setup
    // while configuration is incomplete.
    let hash = passwords::hash_password("setup-password").await.unwrap();
    db::users::accounts::create_user_with_password_and_flags(
        fixture.pool(),
        "admin",
        None,
        &hash,
        true,
        false,
    )
    .await
    .unwrap();
    let (status, _, body) = post(
        &router,
        "/api/auth/setup",
        &session,
        Some(&csrf),
        "http://forge.example.test",
        setup("admin"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "recovery: {body}");
    assert!(db::node_config::get_node_configuration(fixture.pool())
        .await
        .unwrap()
        .is_some_and(|config| config.setup_completed));
    fixture.finish().await;
}

async fn native_auth_forms_contract(pg: bool) {
    let _serial = SERIAL.lock().await;
    fake_webui_bundle::install();
    for provider in ["GITHUB", "GOOGLE", "GITLAB", "DISCORD", "BITBUCKET"] {
        std::env::remove_var(format!("{provider}_CLIENT_ID"));
        std::env::remove_var(format!("{provider}_CLIENT_SECRET"));
    }
    // A provider needs both values, so GitHub with only an id stays hidden.
    for provider in ["GITLAB", "DISCORD", "BITBUCKET"] {
        std::env::set_var(
            format!("{provider}_CLIENT_ID"),
            format!("{provider}-client"),
        );
        std::env::set_var(format!("{provider}_CLIENT_SECRET"), "provider-secret");
    }
    std::env::set_var("GITHUB_CLIENT_ID", "github-client");

    let mut fixture = Database::new(pg).await;
    fixture.execute("DELETE FROM repositories", &[]).await;
    fixture.execute("DELETE FROM users", &[]).await;
    let hash = passwords::hash_password("correct-password").await.unwrap();
    assert!(matches!(
        transitions::complete_setup(fixture.pool(), "root_admin", None, &hash, "public", None)
            .await
            .unwrap(),
        SetupClaim::Completed(_)
    ));
    db::users::accounts::create_user_with_password_and_flags(
        fixture.pool(),
        "throttled_user",
        None,
        &hash,
        false,
        false,
    )
    .await
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let router = test_router(&fixture, root.path()).await;
    let (session, csrf) = session_with_csrf(fixture.pool(), None).await;

    for path in ["/login", "/register"] {
        let (status, _, body) = get(&router, path, Some(&session)).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        for provider in ["gitlab", "discord", "bitbucket"] {
            let link = format!("href=\"/api/auth/oauth/{provider}\"");
            assert!(body.contains(&link), "{path} missing {provider}");
        }
        for provider in ["github", "google"] {
            let link = format!("href=\"/api/auth/oauth/{provider}\"");
            assert!(
                !body.contains(&link),
                "{path} shows unconfigured {provider}"
            );
        }
    }

    // The bounded password policy names its limit on the native register form.
    let long_password = "a".repeat(1200);
    let (status, body) = native_form(
        &router,
        "/register",
        &session,
        &[
            ("csrf_token", csrf.as_str()),
            ("mode", "signup"),
            ("username", "long_password_user"),
            ("password", long_password.as_str()),
            ("confirm_password", long_password.as_str()),
            ("redirect", "/"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.contains("Password must be between 8 and 1024 bytes."),
        "{body}"
    );

    // Ten wrong passwords use the account budget; the next attempt is throttled.
    let attempt = [
        ("csrf_token", csrf.as_str()),
        ("mode", "signin"),
        ("username", "throttled_user"),
        ("password", "wrong-password"),
    ];
    for _ in 0..10 {
        let (status, body) = native_form(&router, "/login", &session, &attempt).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert!(body.contains("Invalid username or password."), "{body}");
    }
    let (status, body) = native_form(&router, "/login", &session, &attempt).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(
        body.contains("Too many attempts. Try again later."),
        "{body}"
    );
    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_temporary_password_settings_are_restricted() {
    temporary_password_settings_contract(false).await;
}

#[tokio::test]
async fn postgres_temporary_password_settings_are_restricted() {
    temporary_password_settings_contract(true).await;
}

/// Wrong bootstrap secrets are charged to the login budget before the secret is compared, so
/// guessing stops when the budget for the name is spent.
async fn setup_secret_guesses_are_limited(pg: bool) {
    let mut fixture = Database::new(pg).await;
    let name = format!("budget_{}", &native::token()[..12]);
    let fresh = native::token();
    let attempt = |secret: &'static str| {
        identity::setup::complete_setup(
            fixture.pool(),
            SetupInput {
                account: PasswordAccount {
                    username: &name,
                    email: None,
                    password: "setup-password",
                },
                node_mode: "public",
                admin_secret: Some(secret),
            },
            Some("bootstrap-secret"),
            SessionContext {
                initiating: InitiatingSession::Missing,
                fresh_id: &fresh,
                user_agent: None,
                expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            },
        )
    };
    for _ in 0..10 {
        assert_eq!(
            attempt("wrong").await.err(),
            Some(SetupError::InvalidAdminSecret)
        );
    }
    assert_eq!(
        attempt("wrong").await.err(),
        Some(SetupError::Admission(AdmissionError::Limited))
    );
    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_setup_secret_guesses_are_limited() {
    setup_secret_guesses_are_limited(false).await;
}

#[tokio::test]
async fn postgres_setup_secret_guesses_are_limited() {
    setup_secret_guesses_are_limited(true).await;
}

#[tokio::test]
async fn sqlite_setup_refuses_new_admin_name_but_recovers_existing_admin() {
    setup_name_contract(false).await;
}

#[tokio::test]
async fn postgres_setup_refuses_new_admin_name_but_recovers_existing_admin() {
    setup_name_contract(true).await;
}

#[tokio::test]
async fn sqlite_native_auth_forms_show_configured_providers_and_public_outcomes() {
    native_auth_forms_contract(false).await;
}

#[tokio::test]
async fn postgres_native_auth_forms_show_configured_providers_and_public_outcomes() {
    native_auth_forms_contract(true).await;
}

async fn appearance_settings_contract(pg: bool) {
    let _serial = SERIAL.lock().await;
    fake_webui_bundle::install();
    let mut fixture = Database::new(pg).await;
    fixture.execute("DELETE FROM repositories", &[]).await;
    fixture.execute("DELETE FROM users", &[]).await;
    let hash = passwords::hash_password("current-password").await.unwrap();
    assert!(matches!(
        transitions::complete_setup(fixture.pool(), "root_admin", None, &hash, "private", None)
            .await
            .unwrap(),
        SetupClaim::Completed(_)
    ));
    let owner = db::users::accounts::create_user_with_password_and_flags(
        fixture.pool(),
        "owner_admin",
        None,
        &hash,
        true,
        false,
    )
    .await
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let router = test_router(&fixture, root.path()).await;
    let (session, _) = session_with_csrf(fixture.pool(), Some(owner.id)).await;

    let (status, _, body) = get(&router, "/settings?tab=appearance", Some(&session)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body.matches("class=\"identity-appearance-row\"").count(),
        3,
        "{body}"
    );
    for value in ["system", "light", "dark"] {
        assert!(
            body.contains(&format!("value=\"{value}\"")),
            "{value}: {body}"
        );
    }
    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_appearance_settings_render_three_theme_forms() {
    appearance_settings_contract(false).await;
}

#[tokio::test]
async fn postgres_appearance_settings_render_three_theme_forms() {
    appearance_settings_contract(true).await;
}
