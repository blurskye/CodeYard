//! All twelve checks operations use the real router and the vendored response schemas.
use crate::support::{database::Database, github_spec::assert_github_schema, http_client::Client};
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use codeyard::backend::{
    db,
    identity::tokens::{self, TokenInput},
    repositories,
};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn send(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
    expected: StatusCode,
) -> (HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(
            request
                .body(if body.is_null() {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("{status}: {}", String::from_utf8_lossy(&bytes)))
    };
    assert_eq!(status, expected, "{operation} {path}: {body}");
    if status.is_success() {
        assert_github_schema(operation, status.as_u16(), &body);
    } else {
        assert!(body["message"].is_string(), "{body}");
    }
    (headers, body)
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let eve = f.actor("eve").await;
    let root = tempfile::tempdir().unwrap();
    let repo = repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "other",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let owner = Client::login(&router, f.pool(), alice).await;
    let writer = Client::login(&router, f.pool(), bob).await;
    let outsider = Client::login(&router, f.pool(), eve).await;
    let mut credentials = Vec::new();
    for (actor, client, scope) in [
        (alice, &owner, "write"),
        (alice, &owner, "read"),
        (bob, &writer, "write"),
        (eve, &outsider, "write"),
        (alice, &owner, "write"),
    ] {
        credentials.push(
            tokens::create(
                f.pool(),
                actor,
                Some(&client.session),
                TokenInput {
                    name: "checks",
                    scope,
                    expires_at: None,
                },
            )
            .await
            .unwrap(),
        );
    }
    let auth = &credentials[0].value;
    let base = "/api/v3/repos/alice/proj";
    let commit = owner.expect("POST","/api/repos/alice/proj/contents/test.rs",json!({"branch":"main","expected_revision":"unborn","message":"Check me","content":STANDARD.encode("one\ntwo\nthree\n")}),StatusCode::CREATED).await;
    let sha = commit["sha"].as_str().unwrap();
    let (_, suite) = send(
        &router,
        "checks/create-suite",
        "POST",
        &format!("{base}/check-suites"),
        Some(auth),
        json!({"head_sha":sha}),
        StatusCode::CREATED,
    )
    .await;
    let suite_id = suite["id"].as_i64().unwrap();
    assert_eq!(suite["head_branch"], "main");
    assert_eq!(suite["status"], "queued");
    let (_, existing) = send(
        &router,
        "checks/create-suite",
        "POST",
        &format!("{base}/check-suites"),
        Some(auth),
        json!({"head_sha":sha.to_ascii_uppercase()}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(existing["id"], suite_id);
    for invalid in ["main".to_owned(), "f".repeat(40)] {
        send(
            &router,
            "checks/create-suite",
            "POST",
            &format!("{base}/check-suites"),
            Some(auth),
            json!({"head_sha":invalid}),
            if invalid.len() == 40 {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            },
        )
        .await;
    }
    let annotation = json!({"path":"test.rs","start_line":1,"end_line":1,"start_column":1,"end_column":3,"annotation_level":"warning","title":"Lint","message":"Please fix this","raw_details":"details"});
    let (_,run) = send(&router,"checks/create","POST",&format!("{base}/check-runs"),Some(auth),json!({"head_sha":sha,"name":"lint","status":"in_progress","started_at":"2026-01-01T00:00:00Z","external_id":"job-1","details_url":"https://example.com/checks/1","output":{"title":"Lint results","summary":"**Checking**","annotations":[annotation.clone(),annotation.clone()]},"actions":[{"label":"Fix","description":"Fix lint","identifier":"fix"}]}),StatusCode::CREATED).await;
    let id = run["id"].as_i64().unwrap();
    let path = format!("{base}/check-runs/{id}");
    assert_eq!(run["check_suite"]["id"], suite_id);
    assert_eq!(run["output"]["annotations_count"], 2);
    let (_, found) = send(
        &router,
        "checks/get",
        "GET",
        &path,
        Some(&credentials[1].value),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(found, run);
    let (_, found) = send(
        &router,
        "checks/get-suite",
        "GET",
        &format!("{base}/check-suites/{suite_id}"),
        Some(auth),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(found["status"], "in_progress");
    assert_eq!(found["latest_check_runs_count"], 1);
    for token in [
        None,
        Some(credentials[1].value.as_str()),
        Some(credentials[2].value.as_str()),
        Some(credentials[3].value.as_str()),
    ] {
        let expected = match token {
            None => StatusCode::UNAUTHORIZED,
            Some(t) if t == credentials[3].value => StatusCode::NOT_FOUND,
            _ => StatusCode::FORBIDDEN,
        };
        send(
            &router,
            "checks/update",
            "PATCH",
            &path,
            token,
            json!({"conclusion":"success"}),
            expected,
        )
        .await;
    }
    for patch in [
        json!({"status":"completed"}),
        json!({"conclusion":"stale"}),
        json!({"status":"waiting"}),
        json!({"details_url":"javascript:alert(1)"}),
        json!({"actions":[{"label":"much too long for a label","description":"test","identifier":"x"}]}),
        json!({"output":{"summary":"bad location","annotations":[{"path":"test.rs","start_line":1,"end_line":2,"start_column":1,"end_column":2,"message":"bad","annotation_level":"failure"}]}}),
        json!({"output":{"summary":"too many","annotations":vec![annotation.clone();51]}}),
    ] {
        send(
            &router,
            "checks/update",
            "PATCH",
            &path,
            Some(auth),
            patch,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let (_,updated) = send(&router,"checks/update","PATCH",&path,Some(auth),json!({"conclusion":"failure","completed_at":"2026-01-01T00:01:00Z","output":{"summary":"Found a problem","annotations":[annotation.clone()]}}),StatusCode::OK).await;
    assert_eq!(updated["status"], "completed");
    assert_eq!(updated["output"]["title"], "Lint results");
    assert_eq!(updated["output"]["annotations_count"], 3);
    let (headers, annotations) = send(
        &router,
        "checks/list-annotations",
        "GET",
        &format!("{path}/annotations?per_page=2"),
        Some(auth),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(annotations.as_array().unwrap().len(), 2);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    assert!(annotations[0]["blob_href"].as_str().unwrap().contains(sha));
    let (_, annotations) = send(
        &router,
        "checks/list-annotations",
        "GET",
        &format!("{path}/annotations?per_page=2&page=2"),
        Some(auth),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(annotations.as_array().unwrap().len(), 1);
    // Concurrent annotation batches append under the same suite lock.
    let patch = json!({"output":{"summary":"More results","annotations":[annotation.clone()]}});
    let (a, b) = tokio::join!(
        send(
            &router,
            "checks/update",
            "PATCH",
            &path,
            Some(auth),
            patch.clone(),
            StatusCode::OK
        ),
        send(
            &router,
            "checks/update",
            "PATCH",
            &path,
            Some(auth),
            patch,
            StatusCode::OK
        )
    );
    assert!(a.1["output"]["annotations_count"].as_i64().unwrap() >= 4);
    assert!(b.1["output"]["annotations_count"].as_i64().unwrap() >= 4);
    let (_, found) = send(
        &router,
        "checks/get",
        "GET",
        &path,
        Some(auth),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(found["output"]["annotations_count"], 5);
    for name in ["lint", "build"] {
        send(
            &router,
            "checks/create",
            "POST",
            &format!("{base}/check-runs"),
            Some(auth),
            json!({"head_sha":sha,"name":name,"conclusion":"success"}),
            StatusCode::CREATED,
        )
        .await;
    }
    for (operation, url, key, count) in [
        (
            "checks/list-for-ref",
            format!("{base}/commits/main/check-runs"),
            "check_runs",
            2,
        ),
        (
            "checks/list-for-ref",
            format!("{base}/commits/{sha}/check-runs?filter=all"),
            "check_runs",
            3,
        ),
        (
            "checks/list-for-suite",
            format!("{base}/check-suites/{suite_id}/check-runs?check_name=lint"),
            "check_runs",
            1,
        ),
        (
            "checks/list-suites-for-ref",
            format!("{base}/commits/main/check-suites?check_name=lint"),
            "check_suites",
            1,
        ),
        (
            "checks/list-for-ref",
            format!("{base}/commits/main/check-runs?app_id=123456"),
            "check_runs",
            0,
        ),
    ] {
        let (_, body) = send(
            &router,
            operation,
            "GET",
            &url,
            Some(auth),
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(body["total_count"], count, "{body}");
        assert_eq!(body[key].as_array().unwrap().len(), count as usize);
    }
    let (headers, body) = send(
        &router,
        "checks/list-for-ref",
        "GET",
        &format!("{base}/commits/main/check-runs?filter=all&per_page=1"),
        Some(auth),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_count"], 3);
    assert_eq!(body["check_runs"].as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("filter=all"));
    send(
        &router,
        "checks/list-for-ref",
        "GET",
        &format!("{base}/commits/main/check-runs?filter=other"),
        Some(auth),
        Value::Null,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    send(
        &router,
        "checks/rerequest-run",
        "POST",
        &format!("{path}/rerequest"),
        Some(&credentials[2].value),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    send(
        &router,
        "checks/rerequest-run",
        "POST",
        &format!("{path}/rerequest"),
        Some(auth),
        Value::Null,
        StatusCode::CREATED,
    )
    .await;
    let (_, run_after) = send(
        &router,
        "checks/get",
        "GET",
        &path,
        Some(auth),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        run_after["conclusion"], "failure",
        "Rerequest must not rewrite the run"
    );
    let (_, suite_after) = send(
        &router,
        "checks/get-suite",
        "GET",
        &format!("{base}/check-suites/{suite_id}"),
        Some(auth),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(suite_after["status"], "queued");
    assert!(suite_after["conclusion"].is_null());
    send(
        &router,
        "checks/rerequest-suite",
        "POST",
        &format!("{base}/check-suites/{suite_id}/rerequest"),
        Some(&credentials[2].value),
        Value::Null,
        StatusCode::CREATED,
    )
    .await;
    let app = db::github_apps::register(
        f.pool(),
        alice,
        &owner.session,
        &db::github_apps::App {
            id: 0, owner_id: alice, bot_id: 0, is_public: true,
            client_id: "checks-client".into(), slug: "check-publisher".into(),
            name: "Check publisher".into(), description: String::new(),
            homepage: "https://example.com".into(), public_key: String::new(),
            permissions: "{\"checks\":\"write\"}".into(), events: "[]".into(),
            access_level: 3, created_at: chrono::Utc::now(),
        },
        "secret-hash",
    )
    .await
    .unwrap()
    .unwrap();
    let pref = format!("{base}/check-suites/preferences");
    let (_, prefs) = send(
        &router,
        "checks/set-suites-preferences",
        "PATCH",
        &pref,
        Some(auth),
        json!({"auto_trigger_checks":[{"app_id":app.id,"setting":false}]}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        prefs["preferences"]["auto_trigger_checks"][0]["setting"],
        false
    );
    let (_, prefs) = send(
        &router,
        "checks/set-suites-preferences",
        "PATCH",
        &pref,
        Some(auth),
        json!({}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        prefs["preferences"]["auto_trigger_checks"][0]["app_id"],
        app.id
    );
    send(
        &router,
        "checks/set-suites-preferences",
        "PATCH",
        &pref,
        Some(&credentials[2].value),
        json!({"auto_trigger_checks":[]}),
        StatusCode::FORBIDDEN,
    )
    .await;
    send(&router,"checks/set-suites-preferences","PATCH",&pref,Some(auth),json!({"auto_trigger_checks":[{"app_id":app.id,"setting":true},{"app_id":999999,"setting":false}]}),StatusCode::UNPROCESSABLE_ENTITY).await;
    let (_, prefs) = send(
        &router,
        "checks/set-suites-preferences",
        "PATCH",
        &pref,
        Some(auth),
        json!({}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        prefs["preferences"]["auto_trigger_checks"][0]["setting"], false,
        "Invalid batches must roll back"
    );
    // The publisher is derived from the OAuth credential, never a supplied app_id.
    f.execute(
        &format!(
            "INSERT INTO oauth_app_tokens(token_id,client_id,scopes) VALUES($1,'{}','repo')",
            app.client_id
        ),
        &[credentials[4].token.id],
    )
    .await;
    let (_, app_run) = send(
        &router,
        "checks/create",
        "POST",
        &format!("{base}/check-runs"),
        Some(&credentials[4].value),
        json!({"head_sha":sha,"name":"app check","app_id":999999}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(app_run["app"]["id"], app.id);
    send(
        &router,
        "checks/update",
        "PATCH",
        &format!("{base}/check-runs/{}", app_run["id"]),
        Some(auth),
        json!({"conclusion":"success"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    let (_, filtered) = send(
        &router,
        "checks/list-for-ref",
        "GET",
        &format!("{base}/commits/main/check-runs?app_id={}", app.id),
        Some(auth),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(filtered["total_count"], 1);
    for (op, url) in [
        ("checks/get", path.clone()),
        (
            "checks/get-suite",
            format!("{base}/check-suites/{suite_id}"),
        ),
        ("checks/list-annotations", format!("{path}/annotations")),
    ] {
        send(
            &router,
            op,
            "GET",
            &url.replace("/proj/", "/other/"),
            Some(auth),
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
        send(
            &router,
            op,
            "GET",
            &url,
            Some(&credentials[3].value),
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    let web = format!("/alice/proj/runs/{id}");
    let (status, html) = owner.page(&web).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("Lint results")
            && html.contains("Please fix this")
            && html.contains("Re-run suite"),
        "{html}"
    );
    assert!(!html.contains("secret-hash"));
    let model = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&web)
                .header("cookie", format!("session_id={}", owner.session))
                .header("accept", codeyard::webui::bootstrap::MODEL_CONTENT_TYPE)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(model.status(), StatusCode::OK);
    let model: codeyard::webui::app::PageEnvelope =
        serde_json::from_slice(&to_bytes(model.into_body(), 8 * 1024 * 1024).await.unwrap())
            .unwrap();
    assert!(matches!(
        model.page,
        codeyard::webui::app::Page::Repositories(
            codeyard::webui::pages::repos::page::Page::Check { .. }
        )
    ));
    let result = owner
        .form_reply(
            &format!("{web}/rerequest"),
            &[("repository_id", &repo.id.to_string())],
        )
        .await;
    assert_eq!(result.status, StatusCode::SEE_OTHER, "{}", result.text());
    let rejected = owner
        .form_reply(&format!("{web}/rerequest"), &[("repository_id", "0")])
        .await;
    assert_eq!(rejected.status, StatusCode::CONFLICT);
    tokens::delete(
        f.pool(),
        alice,
        Some(&owner.session),
        credentials[0].token.id,
    )
    .await
    .unwrap();
    send(
        &router,
        "checks/update",
        "PATCH",
        &path,
        Some(auth),
        json!({"conclusion":"success"}),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn sqlite_github_checks() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_checks() {
    contract(true).await;
}
