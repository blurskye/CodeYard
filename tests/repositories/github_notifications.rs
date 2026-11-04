use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};

use codeyard_repos_pulls::issues::comments;
use codeyard_repos_pulls::issues::comments::CreateCommentRequest;
use codeyard_repos_core::repositories::lifecycle;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn call(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
    expected: StatusCode,
) -> Value {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", token);
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
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    if matches!(status, StatusCode::NO_CONTENT | StatusCode::RESET_CONTENT) {
        assert!(bytes.is_empty());
    }
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        let value = serde_json::from_slice(&bytes).unwrap();
        assert_github_schema(operation, status.as_u16(), &value);
        value
    };
    assert_eq!(status, expected, "{operation}: {value}");
    value
}
async fn comment(f: &Database, actor: i64, name: &str, number: i32, body: &str) -> i64 {
    comments::create_issue_comment(
        f.pool(),
        &NotificationBroadcaster::new(f.pool()),
        actor,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(f.pool(), actor).await,
        ),
        "alice".into(),
        name.into(),
        number,
        CreateCommentRequest {
            body: body.into(),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .id
}
async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (name, private) in [("proj", false), ("secret", true)] {
        repos.push(
            lifecycle::provision(
                (f.pool(), None),
                root.path(),
                codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(
                    alice_id,
                ),
                name,
                None,
                private,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let mut credentials = Vec::new();
    let mut token_ids = Vec::new();
    for (id, client, scope) in [
        (bob_id, &bob, "write"),
        (bob_id, &bob, "read"),
        (alice_id, &alice, "write"),
    ] {
        let token = tokens::create(
            f.pool(),
            id,
            Some(&client.session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        token_ids.push(token.token.id);
        credentials.push(format!("Bearer {}", token.value));
    }
    let (writer, reader, owner) = (
        credentials[0].as_str(),
        credentials[1].as_str(),
        credentials[2].as_str(),
    );
    let first = codeyard_db_repos::issues::create_issue(
        f.pool(),
        repos[0].id,
        alice_id,
        "First native topic",
        "",
    )
    .await
    .unwrap();
    let second = codeyard_db_repos::issues::create_issue(
        f.pool(),
        repos[0].id,
        alice_id,
        "Second native topic",
        "",
    )
    .await
    .unwrap();
    let first_comment = comment(
        &f,
        alice_id,
        "proj",
        first.number,
        "@bob first real mention",
    )
    .await;
    comment(
        &f,
        alice_id,
        "proj",
        second.number,
        "@bob second real mention",
    )
    .await;
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob_id,
        repos[1].id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let private = codeyard_db_repos::issues::create_issue(
        f.pool(),
        repos[1].id,
        alice_id,
        "HIDDEN SUBJECT",
        "",
    )
    .await
    .unwrap();
    comment(
        &f,
        alice_id,
        "secret",
        private.number,
        "@bob HIDDEN COMMENT",
    )
    .await;
    codeyard_db_repos::repositories::access::revoke_repo_access(f.pool(), bob_id, repos[1].id)
        .await
        .unwrap();
    const LIST: &str = "activity/list-notifications-for-authenticated-user";
    let values = call(
        &router,
        LIST,
        "GET",
        "/api/v3/notifications?per_page=1",
        Some(reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(values.as_array().unwrap().len(), 1);
    assert!(!values.to_string().contains("HIDDEN"));
    let next = call(
        &router,
        LIST,
        "GET",
        "/api/v3/notifications?per_page=1&page=2",
        Some(reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(next.as_array().unwrap().len(), 1);
    assert_ne!(values[0]["id"], next[0]["id"]);
    let linked = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v3/notifications?per_page=1")
                .header("authorization", reader)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(linked.headers()["link"]
        .to_str()
        .unwrap()
        .contains("rel=\"next\""));
    assert_eq!(linked.headers()["x-poll-interval"], "60");
    for filter in ["since=2099-01-01T00:00:00Z", "before=2000-01-01T00:00:00Z"] {
        assert!(call(
            &router,
            LIST,
            "GET",
            &format!("/api/v3/notifications?{filter}"),
            Some(reader),
            Value::Null,
            StatusCode::OK
        )
        .await
        .as_array()
        .unwrap()
        .is_empty());
    }
    let browser_only = bob
        .exchange(
            "GET",
            "/api/v3/notifications",
            "application/json",
            Vec::new(),
        )
        .await;
    assert_eq!(browser_only.status, StatusCode::UNAUTHORIZED);
    assert!(browser_only.body.is_empty());
    let all = call(
        &router,
        LIST,
        "GET",
        "/api/v3/notifications?all=true",
        Some(writer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(all.as_array().unwrap().len(), 2);
    let topic = all
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["subject"]["title"] == first.title)
        .unwrap();
    let id = topic["id"].as_str().unwrap();
    let thread = format!("/api/v3/notifications/threads/{id}");
    let subscription = format!("{thread}/subscription");
    assert_eq!(topic["subject"]["type"], "Issue");
    assert!(topic["subject"]["url"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/issues/{}", first.number)));
    assert!(topic["subject"]["latest_comment_url"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/issues/comments/{}", first_comment * 2)));
    assert_eq!(topic["reason"], "mention");
    let repo_list = call(
        &router,
        "activity/list-repo-notifications-for-authenticated-user",
        "GET",
        "/api/v3/repos/alice/proj/notifications",
        Some(writer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(repo_list.as_array().unwrap().len(), 2);
    call(
        &router,
        "activity/get-thread",
        "GET",
        &thread,
        Some(owner),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    call(
        &router,
        LIST,
        "GET",
        "/api/v3/notifications",
        None,
        Value::Null,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    call(
        &router,
        LIST,
        "GET",
        "/api/v3/notifications?since=bad",
        Some(writer),
        Value::Null,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    call(
        &router,
        "activity/mark-thread-as-read",
        "PATCH",
        &thread,
        Some(reader),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    call(
        &router,
        "activity/mark-thread-as-read",
        "PATCH",
        &thread,
        Some(writer),
        Value::Null,
        StatusCode::RESET_CONTENT,
    )
    .await;
    let read = call(
        &router,
        "activity/get-thread",
        "GET",
        &thread,
        Some(writer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(read["unread"], false);
    assert!(read["last_read_at"].is_string());
    let browser = codeyard_db_repos::notifications::records::notification_inbox(
        f.pool(),
        &bob.session,
        bob_id,
        codeyard_db_repos::notifications::records::NotificationInboxAction::List {
            filter: codeyard_db_repos::notifications::records::NotificationFilter::inbox(false),
            limit: 100,
            before: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(browser
        .notifications
        .iter()
        .filter(
            |n| n.metadata["number"] == first.number || n.metadata["issue_number"] == first.number
        )
        .all(|n| n.read_at.is_some()));
    let initial = call(
        &router,
        "activity/get-thread-subscription-for-authenticated-user",
        "GET",
        &subscription,
        Some(reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(initial["subscribed"], true);
    call(
        &router,
        "activity/delete-thread-subscription",
        "DELETE",
        &subscription,
        Some(writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let before = codeyard_db_repos::notifications::records::list_notifications_for_user(
        f.pool(),
        bob_id,
        100,
        false,
    )
    .await
    .unwrap()
    .len();
    comment(
        &f,
        alice_id,
        "proj",
        first.number,
        "No mention after unsubscribe",
    )
    .await;
    assert_eq!(
        codeyard_db_repos::notifications::records::list_notifications_for_user(
            f.pool(),
            bob_id,
            100,
            false
        )
        .await
        .unwrap()
        .len(),
        before
    );
    let subscribed = call(
        &router,
        "activity/set-thread-subscription",
        "PUT",
        &subscription,
        Some(writer),
        json!({"ignored":false}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(subscribed["subscribed"], true);
    assert_eq!(subscribed["created_at"], initial["created_at"]);
    comment(
        &f,
        alice_id,
        "proj",
        first.number,
        "Now a real subscribed delivery without a mention",
    )
    .await;
    assert_eq!(
        codeyard_db_repos::notifications::records::list_notifications_for_user(
            f.pool(),
            bob_id,
            100,
            false
        )
        .await
        .unwrap()
        .len(),
        before + 1
    );
    let ignored = call(
        &router,
        "activity/set-thread-subscription",
        "PUT",
        &subscription,
        Some(writer),
        json!({"ignored":true}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(ignored["ignored"], true);
    assert_eq!(ignored["subscribed"], false);
    comment(
        &f,
        alice_id,
        "proj",
        first.number,
        "@bob ignored even when mentioned",
    )
    .await;
    assert_eq!(
        codeyard_db_repos::notifications::records::list_notifications_for_user(
            f.pool(),
            bob_id,
            100,
            false
        )
        .await
        .unwrap()
        .len(),
        before + 1
    );
    call(
        &router,
        "activity/set-thread-subscription",
        "PUT",
        &subscription,
        Some(writer),
        json!({}),
        StatusCode::OK,
    )
    .await;
    comment(
        &f,
        alice_id,
        "proj",
        first.number,
        "@bob resumed exactly once",
    )
    .await;
    assert_eq!(
        codeyard_db_repos::notifications::records::list_notifications_for_user(
            f.pool(),
            bob_id,
            100,
            false
        )
        .await
        .unwrap()
        .len(),
        before + 2
    );
    call(
        &router,
        "activity/mark-repo-notifications-as-read",
        "PUT",
        "/api/v3/repos/alice/proj/notifications",
        Some(writer),
        json!({}),
        StatusCode::RESET_CONTENT,
    )
    .await;
    assert!(call(
        &router,
        LIST,
        "GET",
        "/api/v3/notifications",
        Some(writer),
        Value::Null,
        StatusCode::OK
    )
    .await
    .as_array()
    .unwrap()
    .is_empty());
    call(
        &router,
        "activity/mark-notifications-as-read",
        "PUT",
        "/api/v3/notifications",
        Some(writer),
        json!({"read":false}),
        StatusCode::RESET_CONTENT,
    )
    .await;
    assert_eq!(
        call(
            &router,
            LIST,
            "GET",
            "/api/v3/notifications",
            Some(writer),
            Value::Null,
            StatusCode::OK
        )
        .await
        .as_array()
        .unwrap()
        .len(),
        2
    );
    call(
        &router,
        "activity/mark-notifications-as-read",
        "PUT",
        "/api/v3/notifications",
        Some(writer),
        json!({}),
        StatusCode::RESET_CONTENT,
    )
    .await;
    call(
        &router,
        "activity/mark-thread-as-done",
        "DELETE",
        &thread,
        Some(writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let remaining = call(
        &router,
        LIST,
        "GET",
        "/api/v3/notifications?all=true",
        Some(writer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(remaining.as_array().unwrap().len(), 1);
    let done = call(
        &router,
        "activity/get-thread",
        "GET",
        &thread,
        Some(writer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(done["unread"], false);
    f.execute(
        "UPDATE repositories SET name='renamed' WHERE id=$1",
        &[repos[0].id],
    )
    .await;
    let renamed = call(
        &router,
        "activity/get-thread",
        "GET",
        &thread,
        Some(writer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(renamed["repository"]["full_name"], "alice/renamed");
    assert!(renamed["subject"]["url"]
        .as_str()
        .unwrap()
        .contains("/alice/renamed/"));
    assert!(renamed["subject"]["latest_comment_url"]
        .as_str()
        .unwrap()
        .contains("/repos/alice/renamed/issues/comments/"));
    tokens::delete(f.pool(), bob_id, Some(&bob.session), token_ids[0])
        .await
        .unwrap();
    call(
        &router,
        "activity/get-thread",
        "GET",
        &thread,
        Some(writer),
        Value::Null,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    f.finish().await;
}
#[tokio::test]
async fn sqlite_github_notifications() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_notifications() {
    contract(true).await;
}
