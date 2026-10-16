//! A draft pull request can be taken out of draft by its author and by people with
//! write access, only while it is open, through the JSON API and the page's own
//! form. On SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
#[path = "support/fake_webui_bundle.rs"]
mod bundle;
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
    http::{header, HeaderMap, Request, StatusCode},
    Router,
};
use base64::Engine;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use database::Database;
use pulls::{moved_forward, text, Client};
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc};
use tower::ServiceExt;

const READY: &str = "Ready for review";

async fn refused(client: &Client, number: i64, status: StatusCode, code: &str) {
    let (got, body) = client
        .api(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/ready"),
            Value::Null,
        )
        .await;
    assert_eq!(got, status, "{body}");
    assert_eq!(body["error"], code, "{body}");
}

async fn ready(client: &Client, number: i64) -> Value {
    client
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/ready"),
            Value::Null,
            StatusCode::OK,
        )
        .await
}

async fn contract(pg: bool) {
    // The HTML answers below render full pages, which need the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let carol_id = f.actor("carol").await;
    let dave_id = f.actor("dave").await;
    let root = tempfile::tempdir().unwrap();
    // A public repository: carol and dave can read it, alice owns it.
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    // Alice is trusted to run workflows, so her pull requests can queue CI.
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig {
            trusted_actor_ids: BTreeSet::from([alice_id]),
            ..CiConfig::default()
        },
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let carol = Client::login(&router, f.pool(), carol_id).await;
    let dave = Client::login(&router, f.pool(), dave_id).await;

    let workflow = base64::engine::general_purpose::STANDARD.encode(
        "name: ci\non: [pull_request]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ci\n",
    );
    let main0 = alice.save("main", "README.md", "unborn").await;
    let main1 = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/contents/.github/workflows/ci.yml",
            json!({"content": workflow, "message": "Add CI", "branch": "main", "expected_revision": main0}),
            StatusCode::CREATED,
        )
        .await["sha"]
        .as_str()
        .unwrap()
        .to_owned();
    for (branch, file) in [
        ("one", "one.txt"),
        ("two", "two.txt"),
        ("three", "three.txt"),
        ("four", "four.txt"),
        ("five", "five.txt"),
    ] {
        alice.branch(branch, &main1).await;
        alice.save(branch, file, &main1).await;
    }
    let runs = || async {
        db::ci::list_ci_runs_for_repo(f.pool(), repository.id, 20)
            .await
            .unwrap()
            .len()
    };

    // The author, who can only read the repository, opens a draft. Nobody can merge it.
    let first = carol.open_draft("Carol's draft", "one", "main").await;
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["author"], first["author"]);
    assert_eq!(listed[0]["author"]["username"], "carol");
    for private_field in [
        "email",
        "password_hash",
        "is_banned",
        "must_change_password",
    ] {
        assert!(listed[0]["author"].get(private_field).is_none());
    }
    assert_eq!(
        (first["number"].as_i64(), first["is_draft"].as_bool()),
        (Some(1), Some(true))
    );
    let (status, body) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/1/merge",
            json!({"expected_head": first["head_sha"], "expected_base": first["base_sha"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "unsupported_merge", "{body}");

    // Someone who is neither the author nor a writer cannot take it out of draft.
    refused(&dave, 1, StatusCode::FORBIDDEN, "forbidden").await;
    assert_eq!(alice.pull(1).await["is_draft"], true);

    // The author can, and the pull request moves to a new revision.
    let done = ready(&carol, 1).await;
    assert_eq!(done["is_draft"], false);
    assert_eq!(done["state"], "open");
    assert!(moved_forward(&first, &done));
    assert_eq!(alice.pull(1).await["is_draft"], false);
    // Once ready, it can be merged. A merged pull request is not open any more.
    alice.merge_current(1).await;
    assert_eq!(alice.pull(1).await["state"], "merged");
    refused(&carol, 1, StatusCode::BAD_REQUEST, "invalid_state").await;
    // No such pull request.
    refused(&carol, 99, StatusCode::NOT_FOUND, "not_found").await;

    // A writer who is not the author can also do it. After that it is not a draft
    // any more, so a second request is refused and changes nothing.
    carol.open_draft("Another draft", "two", "main").await;
    let by_writer = ready(&alice, 2).await;
    assert_eq!(by_writer["is_draft"], false);
    assert_eq!(text(&by_writer, "state"), "open");
    refused(&carol, 2, StatusCode::CONFLICT, "invalid_state").await;
    assert_eq!(alice.pull(2).await["updated_at"], by_writer["updated_at"]);

    // Only open pull requests: a closed draft stays a closed draft.
    let third = carol.open_draft("A draft to close", "three", "main").await;
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/pulls/3",
            json!({"expected_state": "open", "expected_updated": third["updated_at"], "state": "closed"}),
            StatusCode::OK,
        )
        .await;
    refused(&carol, 3, StatusCode::BAD_REQUEST, "invalid_state").await;
    refused(&alice, 3, StatusCode::BAD_REQUEST, "invalid_state").await;
    assert_eq!(alice.pull(3).await["is_draft"], true);

    // The page offers the action to the author and to writers, not to others.
    carol.open_draft("Draft on the page", "four", "main").await;
    for (client, offered) in [(&carol, true), (&alice, true), (&dave, false)] {
        let (status, page) = client.page("/alice/proj/pulls/4").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page.contains(READY), offered, "ready button on the page");
    }
    // The form posts back what the page showed; an old copy is refused.
    let shown = carol.pull(4).await;
    let (state, updated) = (text(&shown, "state"), text(&shown, "updated_at"));
    let stale = [
        ("expected_state", state),
        ("expected_updated", "2020-01-01T00:00:00+00:00"),
    ];
    let (status, page) = carol.form("/alice/proj/pulls/4/ready", &stale).await;
    assert_eq!(status, StatusCode::CONFLICT, "{page}");
    assert!(page.contains("changed after the form was opened"), "{page}");
    assert_eq!(alice.pull(4).await["is_draft"], true);
    // A reader who sends the form anyway is refused.
    let fields = [("expected_state", state), ("expected_updated", updated)];
    let (status, page) = dave.form("/alice/proj/pulls/4/ready", &fields).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    assert_eq!(alice.pull(4).await["is_draft"], true);
    // The author's own form works, and the page then drops the button.
    let (status, page) = carol.form("/alice/proj/pulls/4/ready", &fields).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    assert_eq!(alice.pull(4).await["is_draft"], false);
    let (status, page) = carol.page("/alice/proj/pulls/4").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(!page.contains(READY));

    // Workflows for a pull request run once it is ready, not while it is a draft.
    let before = runs().await;
    let fifth = alice.open_draft("Draft with CI", "five", "main").await;
    assert_eq!(fifth["is_draft"], true);
    assert_eq!(runs().await, before, "a draft queues no workflow");
    ready(&alice, 5).await;
    assert_eq!(runs().await, before + 1, "readying queues its workflow");
    assert_eq!(
        db::ci::list_ci_runs_for_repo(f.pool(), repository.id, 20)
            .await
            .unwrap()[0]
            .event,
        "pull_request"
    );

    f.finish().await;
}

async fn request_with_headers(
    router: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    tokens: &[&str],
    extra: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::HOST, "forge.example.test");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, format!("session_id={cookie}"));
    }
    for token in tokens {
        request = request.header("x-csrf-token", *token);
    }
    for &(name, value) in extra {
        request = request.header(name, value);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

async fn cookie_bound_mutations(pg: bool) {
    bundle::install();
    let mut fixture = Database::new(pg).await;
    fixture.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice = fixture.actor("alice").await;
    let bob_id = fixture.actor("bob").await;
    let newcomer = fixture.actor("newcomer").await;
    let hash = codeyard::backend::identity::passwords::hash_password("first-login-password")
        .await
        .unwrap();
    db::users::accounts::update_user_password(fixture.pool(), newcomer, &hash, false)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    codeyard::backend::repositories::lifecycle::provision(
        (fixture.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        fixture.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, fixture.pool(), alice).await;
    let mut bob = Client::login(&router, fixture.pool(), bob_id).await;
    let head = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic", &head).await;
    bob.open_draft("Draft", "topic", "main").await;
    let issue = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Issue with protected comments"}),
            StatusCode::CREATED,
        )
        .await;
    let comments_path = format!(
        "/api/repos/alice/proj/issues/{}/comments",
        issue["number"].as_i64().unwrap()
    );
    let request = Request::builder()
        .method("POST")
        .uri(&comments_path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::HOST, "forge.example.test")
        .header(header::COOKIE, format!("session_id={}", bob.session))
        .body(Body::from(json!({"body": "Protected comment"}).to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "no-store, private"
    );
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"], "csrf_error");
    assert_eq!(
        bob.expect("GET", &comments_path, Value::Null, StatusCode::OK)
            .await,
        json!([])
    );
    let comment = bob
        .expect(
            "POST",
            &comments_path,
            json!({"body": "Protected comment"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(comment["body"], "Protected comment");
    let ready_path = "/api/repos/alice/proj/pulls/1/ready";
    let fork_path = "/api/repos/alice/proj/forks";
    let cookie = Some(bob.session.as_str());
    for (case, tokens, extra) in [
        ("missing", vec![], vec![]),
        ("empty", vec![""], vec![]),
        ("wrong", vec![alice.csrf.as_str()], vec![]),
        (
            "duplicate",
            vec![bob.csrf.as_str(), bob.csrf.as_str()],
            vec![],
        ),
        (
            "foreign",
            vec![bob.csrf.as_str()],
            vec![("origin", "https://attacker.invalid")],
        ),
        (
            "sibling",
            vec![bob.csrf.as_str()],
            vec![
                ("origin", "http://sibling.example.test"),
                ("sec-fetch-site", "same-site"),
            ],
        ),
        (
            "referer",
            vec![bob.csrf.as_str()],
            vec![("referer", "http://sibling.example.test/page")],
        ),
        (
            "fetch",
            vec![bob.csrf.as_str()],
            vec![("sec-fetch-site", "cross-site")],
        ),
    ] {
        for path in [ready_path, fork_path] {
            let (status, headers, body) =
                request_with_headers(&router, "POST", path, cookie, &tokens, &extra).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{case} {path}: {body}");
            assert_eq!(body["error"], "csrf_error");
            assert_eq!(headers[header::CACHE_CONTROL], "no-store, private");
        }
        assert_eq!(bob.pull(1).await["is_draft"], true);
        assert_eq!(
            bob.api("GET", "/api/repos/bob/proj", Value::Null).await.0,
            StatusCode::NOT_FOUND
        );
    }
    match &fixture {
        Database::Postgres(f) => {
            sqlx::query("UPDATE sessions SET csrf_expires_at='2000-01-01T00:00:00Z' WHERE id=$1")
                .bind(&bob.session)
                .execute(&f.raw)
                .await
                .unwrap();
        }
        Database::Sqlite(f) => {
            sqlx::query("UPDATE sessions SET csrf_expires_at='2000-01-01T00:00:00Z' WHERE id=$1")
                .bind(&bob.session)
                .execute(&f.raw)
                .await
                .unwrap();
        }
    }
    let (status, _, body) =
        request_with_headers(&router, "POST", ready_path, cookie, &[&bob.csrf], &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "stale CSRF: {body}");
    assert_eq!(bob.pull(1).await["is_draft"], true);
    bob.csrf = db::users::sessions::get_or_create_csrf_token(
        fixture.pool(),
        &bob.session,
        Some(bob_id),
        &native::token(),
        3600,
    )
    .await
    .unwrap()
    .unwrap();
    let (status, _, body) = request_with_headers(
        &router,
        "POST",
        ready_path,
        None,
        &[],
        &[("authorization", "Bearer token-without-cookie")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "Authorization clients must not see CSRF errors: {body}"
    );
    let (status, _, body) =
        request_with_headers(&router, "POST", "/api/ci/trigger", cookie, &[], &[]).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "CI trigger needs the browser token: {body}"
    );
    assert_eq!(body["error"], "csrf_error");
    let (status, _, body) = request_with_headers(
        &router,
        "POST",
        ready_path,
        cookie,
        &[&bob.csrf],
        &[("origin", "http://forge.example.test")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(bob.pull(1).await["is_draft"], false);
    let (status, _, body) = request_with_headers(
        &router,
        "POST",
        fork_path,
        cookie,
        &[&bob.csrf],
        &[("origin", "http://forge.example.test")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        bob.api("GET", "/api/repos/bob/proj", Value::Null).await.0,
        StatusCode::OK
    );

    for (cookie, tokens, extra) in [
        (None, vec![], vec![("origin", "http://attacker.invalid")]),
        (None, vec![], vec![("sec-fetch-site", "cross-site")]),
        (
            Some(bob.session.as_str()),
            vec![bob.csrf.as_str()],
            vec![
                ("origin", "http://sibling.example.test"),
                ("sec-fetch-site", "same-site"),
            ],
        ),
        (
            Some(bob.session.as_str()),
            vec![bob.csrf.as_str()],
            vec![("referer", "http://sibling.example.test/page")],
        ),
        (
            Some(bob.session.as_str()),
            vec![],
            vec![("origin", "http://forge.example.test")],
        ),
        (None, vec![], vec![("origin", "http://forge.example.test")]),
    ] {
        let (status, headers, _) =
            request_with_headers(&router, "POST", "/api/auth/logout", cookie, &tokens, &extra)
                .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(headers.get(header::SET_COOKIE).is_none());
        assert!(
            db::users::sessions::get_session_user(fixture.pool(), &bob.session)
                .await
                .unwrap()
                .is_some()
        );
    }
    let (status, headers, body) = request_with_headers(
        &router,
        "POST",
        "/api/auth/logout",
        Some(&bob.session),
        &[&bob.csrf],
        &[("origin", "http://forge.example.test")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(headers.get(header::SET_COOKIE).is_some());
    assert!(
        db::users::sessions::get_session_user(fixture.pool(), &bob.session)
            .await
            .unwrap()
            .is_none()
    );
    for path in [
        "/api/auth/session",
        "/api/auth/login",
        "/api/auth/register",
        "/api/auth/setup",
        "/api/auth/logout",
    ] {
        let (status, headers, body) = request_with_headers(
            &router,
            "POST",
            path,
            None,
            &[],
            &[("origin", "http://attacker.invalid")],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
        assert!(headers.get(header::SET_COOKIE).is_none());
    }
    let (status, headers, _) = request_with_headers(
        &router,
        "POST",
        "/api/auth/session",
        None,
        &[],
        &[("origin", "http://forge.example.test")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(headers.get(header::SET_COOKIE).is_some());
    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .header("origin", "http://forge.example.test")
        .header(header::HOST, "forge.example.test")
        .body(Body::from(
            serde_json::json!({"username":"newcomer", "password":"first-login-password"})
                .to_string(),
        ))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "initial login without a prior session"
    );
    assert!(response.headers().get(header::SET_COOKIE).is_some());
    let (status, _, body) = request_with_headers(
        &router,
        "POST",
        "/api/auth/session",
        Some(&alice.session),
        &[],
        &[],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "existing cookie needs its CSRF token: {body}"
    );
    let (status, _, body) = request_with_headers(
        &router,
        "POST",
        "/api/auth/session",
        Some(&alice.session),
        &[&alice.csrf],
        &[("origin", "http://forge.example.test")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, headers, body) = request_with_headers(
        &router,
        "GET",
        "/api/auth/me",
        Some(&alice.session),
        &[],
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store, private");
    let (status, headers, body) =
        request_with_headers(&router, "GET", "/api/auth/setup-status", None, &[], &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(headers.get(header::CACHE_CONTROL).is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_cookie_bound_mutations() {
    cookie_bound_mutations(false).await;
}

#[tokio::test]
async fn postgres_cookie_bound_mutations() {
    cookie_bound_mutations(true).await;
}

#[tokio::test]
async fn sqlite_draft_pull_requests_become_ready() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_draft_pull_requests_become_ready() {
    contract(true).await;
}
