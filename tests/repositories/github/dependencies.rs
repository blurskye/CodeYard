use super::*;
use codeyard_ci_engine::runs::CiRuns;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_repos_pulls::repositories::dependencies::registry;
use codeyard_repos_pulls::repositories::dependencies::worker;
mod provider;
mod workers;
struct World {
    f: Database,
    _root: tempfile::TempDir,
    router: Router,
    state: AppState,
    ci: Arc<CiRuns>,
    broadcaster: Arc<NotificationBroadcaster>,
    owner: i64,
    repo: codeyard_db_core::models::Repository,
    writer: String,
    client: Client,
}
async fn setup(pg: bool) -> World {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let ci = state.ci_runs.clone();
    let broadcaster = state.notification_broadcaster.clone();
    let router = create_router(state.clone());
    let writer = credential(&router, &f, owner, "write").await;
    let (_, created) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"deps","private":true,"auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let repo = f.repository(created["id"].as_i64().unwrap()).await;
    let client = Client::login(&router, f.pool(), owner).await;
    World {
        f,
        _root: root,
        router,
        state,
        ci,
        broadcaster,
        owner,
        repo,
        writer,
        client,
    }
}
const BASE: &str = "/api/v3/repos/alice/deps";
async fn enable(w: &World, token: &str) {
    for feature in ["vulnerability-alerts", "automated-security-fixes"] {
        let operation = if feature == "vulnerability-alerts" {
            "repos/enable-vulnerability-alerts"
        } else {
            "repos/enable-automated-security-fixes"
        };
        checked(
            &w.router,
            operation,
            "PUT",
            &format!("{BASE}/{feature}"),
            Some(token),
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    }
}
async fn fixes(w: &World) -> Value {
    checked(
        &w.router,
        "repos/check-automated-security-fixes",
        "GET",
        &format!("{BASE}/automated-security-fixes"),
        Some(&w.writer),
        Value::Null,
        StatusCode::OK,
    )
    .await
    .1
}
async fn controls(pg: bool) {
    let w = setup(pg).await;
    let read = credential(&w.router, &w.f, w.owner, "read").await;
    let bob = w.f.actor("bob").await;
    codeyard_db_repos::repositories::access::grant_repo_access(
        w.f.pool(),
        bob,
        w.repo.id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let outsider = credential(&w.router, &w.f, bob, "write").await;
    let alerts = format!("{BASE}/vulnerability-alerts");
    let updates = format!("{BASE}/automated-security-fixes");
    checked(
        &w.router,
        "repos/check-vulnerability-alerts",
        "GET",
        &alerts,
        Some(&read),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(fixes(&w).await, json!({"enabled":false,"paused":false}));
    checked(
        &w.router,
        "repos/enable-automated-security-fixes",
        "PUT",
        &updates,
        Some(&w.writer),
        Value::Null,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &w.router,
        "repos/enable-vulnerability-alerts",
        "PUT",
        &alerts,
        Some(&read),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &w.router,
        "repos/check-automated-security-fixes",
        "GET",
        &updates,
        Some(&outsider),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let other = Client::login(&w.router, w.f.pool(), bob).await;
    assert_eq!(
        other.page("/alice/deps/security/dependencies").await.0,
        StatusCode::NOT_FOUND
    );
    let id = w.repo.id.to_string();
    let action = "/alice/deps/security/dependencies/alerts/enable";
    let (status, html) = w
        .client
        .form(action, &[("repository_id", &id), ("confirmation", "deps")])
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("Private dependency export consent"));
    assert_eq!(
        w.client
            .form(
                action,
                &[
                    ("repository_id", "999999"),
                    ("confirmation", "deps"),
                    ("consent", "on")
                ]
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let body = format!("repository_id={id}&confirmation=deps&consent=on");
    let response = w
        .router
        .clone()
        .oneshot(
            Request::post(action)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", format!("session_id={}", w.client.session))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        w.client
            .form(
                action,
                &[
                    ("repository_id", &id),
                    ("confirmation", "deps"),
                    ("consent", "on")
                ]
            )
            .await
            .0,
        StatusCode::SEE_OTHER
    );
    checked(
        &w.router,
        "repos/check-vulnerability-alerts",
        "GET",
        &alerts,
        Some(&read),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    enable(&w, &w.writer).await;
    assert_eq!(fixes(&w).await, json!({"enabled":true,"paused":false}));
    let provider = provider::Provider::new().await;
    let client = registry::Client::new(&provider.origin).unwrap();
    assert!(worker::run_once(w.f.pool(), &w.ci, &w.broadcaster, &client)
        .await
        .unwrap());
    assert_eq!(
        provider.queries.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    // Only update proposals need npm files, so a repository without them is not paused. The
    // shared scanner has not read this commit yet, so the job stops and is retried.
    assert_eq!(fixes(&w).await, json!({"enabled":true,"paused":false}));
    let html = w.client.page_ok("/alice/deps/security/dependencies").await;
    assert!(html.contains("verify root npm files"));
    assert!(html.contains("MAY execute"));
    assert!(!html.contains("lease_nonce"));
    checked(
        &w.router,
        "repos/disable-automated-security-fixes",
        "DELETE",
        &updates,
        Some(&w.writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(fixes(&w).await, json!({"enabled":false,"paused":false}));
    checked(
        &w.router,
        "repos/disable-vulnerability-alerts",
        "DELETE",
        &alerts,
        Some(&w.writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &w.router,
        "repos/check-vulnerability-alerts",
        "GET",
        &alerts,
        Some(&read),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &w.router,
        "repos/update",
        "PATCH",
        BASE,
        Some(&w.writer),
        json!({"archived":true}),
        StatusCode::OK,
    )
    .await;
    checked(
        &w.router,
        "repos/enable-vulnerability-alerts",
        "PUT",
        &alerts,
        Some(&w.writer),
        Value::Null,
        StatusCode::CONFLICT,
    )
    .await;
    let html = w.client.page_ok("/alice/deps/security/dependencies").await;
    assert!(html.contains("controls are read-only"));
    assert!(!html.contains("action=\"/alice/deps/security/dependencies/alerts/enable\""));
}
#[tokio::test]
async fn dependency_controls_and_native_forms_on_both_engines() {
    controls(false).await;
    controls(true).await;
}
