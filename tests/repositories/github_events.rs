use crate::support::{database::Database, github_spec::assert_github_schema, http_client::Client};
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use codeyard::backend::{
    db,
    identity::tokens::{self, TokenInput},
    notifications::NotificationBroadcaster,
    repositories::{
        engagement::{self, Engagement},
        lifecycle,
    },
};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn get(
    router: &Router,
    operation: &str,
    path: &str,
    token: Option<&str>,
    etag: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .uri(path)
        .header("accept", "application/vnd.github+json");
    if let Some(token) = token {
        request = request.header("authorization", token);
    }
    if let Some(etag) = etag {
        request = request.header("if-none-match", etag);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        let value = serde_json::from_slice(&bytes).unwrap();
        assert_github_schema(operation, status.as_u16(), &value);
        value
    };
    (status, headers, value)
}
async fn atom_feed(router: &Router, url: &str) -> (StatusCode, String) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(url)
                .header("accept", "application/atom+xml")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    if status == StatusCode::OK {
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/atom+xml"));
    }
    (
        status,
        String::from_utf8(
            to_bytes(response.into_body(), 8 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap(),
    )
}
async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
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
    let private = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "secret",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let mut credentials = Vec::new();
    for (id, client) in [(alice_id, &alice), (bob_id, &bob)] {
        let credential = tokens::create(
            f.pool(),
            id,
            Some(&client.session),
            TokenInput {
                name: "events",
                scope: "write",
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", credential.value));
    }
    let owner = credentials[0].as_str();
    let visitor = credentials[1].as_str();
    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature", &base).await;
    alice.save("feature", "feature.txt", &base).await;
    let pull = alice.open_pull("Feature", "feature", "main").await;
    let number = pull["number"].as_i64().unwrap();
    alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/comments"),
            json!({"body":"Public conversation"}),
            StatusCode::CREATED,
        )
        .await;
    let broadcaster = NotificationBroadcaster::new(f.pool());
    engagement::change(
        f.pool(),
        &broadcaster,
        bob_id,
        repo.id,
        Engagement::Star,
        true,
    )
    .await
    .unwrap();
    db::following::follow_user(
        f.pool(),
        bob_id,
        alice_id,
        &db::users::accounts::AccountCredential::Session(
            &crate::support::live_session(f.pool(), bob_id).await,
        ),
    )
    .await
    .unwrap();
    let org = db::organizations::profiles::create_organization(
        f.pool(),
        alice_id,
        &db::organizations::profiles::NewOrganization::new(
            "acme",
            db::organizations::profiles::OrganizationProfile::new(Some("Acme"), None).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let db::organizations::OrganizationOutcome::Applied(org) = org else {
        panic!("organization creation failed")
    };
    let _org_repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner {
            actor_id: alice_id,
            target: db::repositories::creation::RepositoryTarget::Organization(org.id),
        },
        "team-project",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    // No hooks are configured: normal domain writes still persist real event payloads.
    let (status, headers, public) = get(
        &router,
        "activity/list-public-events",
        "/api/v3/events?per_page=100",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{public}");
    let events = public.as_array().unwrap();
    for kind in [
        "CreateEvent",
        "PushEvent",
        "PullRequestEvent",
        "IssueCommentEvent",
        "WatchEvent",
    ] {
        assert!(
            events.iter().any(|e| e["type"] == kind),
            "missing {kind}: {public}"
        );
    }
    assert!(events
        .iter()
        .all(|e| e["public"] == true && !e["repo"]["name"].as_str().unwrap().contains("secret")));
    assert_eq!(headers["x-poll-interval"], "60");
    let etag = headers["etag"].to_str().unwrap();
    let (status, _, body) = get(
        &router,
        "activity/list-public-events",
        "/api/v3/events?per_page=100",
        None,
        Some(etag),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_null());
    for (operation, path, credential) in [
        (
            "activity/list-repo-events",
            "/api/v3/repos/alice/proj/events",
            None,
        ),
        (
            "activity/list-public-events-for-repo-network",
            "/api/v3/networks/alice/proj/events",
            None,
        ),
        (
            "activity/list-public-org-events",
            "/api/v3/orgs/acme/events",
            None,
        ),
        (
            "activity/list-org-events-for-authenticated-user",
            "/api/v3/users/alice/events/orgs/acme",
            Some(owner),
        ),
        (
            "activity/list-events-for-authenticated-user",
            "/api/v3/users/alice/events",
            Some(owner),
        ),
        (
            "activity/list-public-events-for-user",
            "/api/v3/users/alice/events/public",
            None,
        ),
        (
            "activity/list-received-events-for-user",
            "/api/v3/users/bob/received_events",
            Some(visitor),
        ),
        (
            "activity/list-received-public-events-for-user",
            "/api/v3/users/bob/received_events/public",
            None,
        ),
    ] {
        let (status, _, body) = get(&router, operation, path, credential, None).await;
        assert_eq!(status, StatusCode::OK, "{operation}: {body}");
        assert!(!body.as_array().unwrap().is_empty(), "{operation}");
        if operation == "activity/list-events-for-authenticated-user" {
            assert!(body
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["repo"]["name"] == "alice/secret"));
        }
        if operation == "activity/list-public-events-for-user" {
            assert!(body
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["repo"]["name"] != "alice/secret"));
        }
    }
    let (_, _, foreign) = get(
        &router,
        "activity/list-events-for-authenticated-user",
        "/api/v3/users/alice/events",
        Some(visitor),
        None,
    )
    .await;
    assert!(foreign
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["repo"]["name"] != "alice/secret"));
    // A hidden repository does not consume another user's visible pagination window.
    let (_, _, page) = get(
        &router,
        "activity/list-events-for-authenticated-user",
        "/api/v3/users/alice/events?per_page=1",
        Some(visitor),
        None,
    )
    .await;
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert_ne!(page[0]["repo"]["id"], private.id);
    let (status, _, feeds) = get(
        &router,
        "activity/get-feeds",
        "/api/v3/feeds",
        Some(owner),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{feeds}");
    let (_, _, same) = get(
        &router,
        "activity/get-feeds",
        "/api/v3/feeds",
        Some(owner),
        None,
    )
    .await;
    assert_eq!(
        feeds["current_user_actor_url"],
        same["current_user_actor_url"]
    );
    for key in [
        "timeline_url",
        "current_user_public_url",
        "current_user_url",
        "current_user_actor_url",
    ] {
        let (status, body) = atom_feed(&router, feeds[key].as_str().unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{key}: {body}");
        assert!(body.starts_with("<?xml"));
        assert!(body.contains("http://www.w3.org/2005/Atom"));
        if key == "current_user_actor_url" {
            assert!(body.contains("alice/secret"));
        } else if matches!(key, "timeline_url" | "current_user_public_url") {
            assert!(!body.contains("alice/secret"));
        }
        assert!(
            !body.contains("feed_"),
            "private feed credentials escaped into an Atom document"
        );
    }
    for url in feeds["current_user_organization_urls"].as_array().unwrap() {
        assert_eq!(
            atom_feed(&router, url.as_str().unwrap()).await.0,
            StatusCode::OK
        );
    }
    let (_, _, anonymous) = get(&router, "activity/get-feeds", "/api/v3/feeds", None, None).await;
    assert!(anonymous.get("current_user_url").is_none());
    for _ in 0..305 {
        engagement::change(
            f.pool(),
            &broadcaster,
            bob_id,
            repo.id,
            Engagement::Star,
            false,
        )
        .await
        .unwrap();
        engagement::change(
            f.pool(),
            &broadcaster,
            bob_id,
            repo.id,
            Engagement::Star,
            true,
        )
        .await
        .unwrap();
    }
    for page in 1..=4 {
        let (status, headers, body) = get(
            &router,
            "activity/list-repo-events",
            &format!("/api/v3/repos/alice/proj/events?per_page=100&page={page}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body.as_array().unwrap().len(),
            if page <= 3 { 100 } else { 0 }
        );
        assert_eq!(
            headers
                .get("link")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("rel=\"next\"")),
            page < 3
        );
    }
    f.execute(
        "UPDATE repositories SET name='renamed' WHERE id=$1",
        &[repo.id],
    )
    .await;
    let (_, _, renamed) = get(
        &router,
        "activity/list-repo-events",
        "/api/v3/repos/alice/renamed/events",
        None,
        None,
    )
    .await;
    assert!(renamed
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["repo"]["name"] == "alice/renamed"));
    f.execute(
        "UPDATE repositories SET is_private=TRUE WHERE id=$1",
        &[repo.id],
    )
    .await;
    let (_, _, hidden) = get(
        &router,
        "activity/list-public-events",
        "/api/v3/events?per_page=100",
        Some(visitor),
        None,
    )
    .await;
    assert!(hidden
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["repo"]["id"] != repo.id));
    let (_, _, own) = get(
        &router,
        "activity/list-repo-events",
        "/api/v3/repos/alice/renamed/events",
        Some(owner),
        None,
    )
    .await;
    assert!(!own.as_array().unwrap().is_empty());
    assert!(own.as_array().unwrap().iter().all(|e| e["public"] == false));
    f.execute("UPDATE events SET created_at='2000-01-01T00:00:00.000000Z' WHERE id=(SELECT MIN(id) FROM events)",&[]).await;
    assert_eq!(db::events::prune_events(f.pool()).await.unwrap(), 1);
    let private_url = feeds["current_user_actor_url"].as_str().unwrap();
    let forged = private_url.replacen("/alice/", "/bob/", 1);
    assert_eq!(atom_feed(&router, &forged).await.0, StatusCode::NOT_FOUND);
    f.execute("UPDATE personal_access_tokens SET expires_at='2000-01-01T00:00:00.000000Z' WHERE user_id=$1",&[alice_id]).await;
    assert_eq!(
        atom_feed(&router, private_url).await.0,
        StatusCode::NOT_FOUND
    );
    f.finish().await;
}
#[tokio::test]
async fn sqlite_github_events() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_events() {
    contract(true).await;
}
