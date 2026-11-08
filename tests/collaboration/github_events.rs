use super::github::{document, request};
use crate::support::database::Database;
use crate::support::http_client::Client;

use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};

async fn events(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_git_storage::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    codeyard_git_storage::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
        "other",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let issue = codeyard_db_repos::issues::create_issue(f.pool(), repo.id, alice, "Original", "")
        .await
        .unwrap();
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for (id, scope) in [(alice, "write"), (alice, "read"), (bob, "write")] {
        let session = Client::login(&router, f.pool(), id).await;
        let token = tokens::create(
            f.pool(),
            id,
            Some(&session.session),
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
    let (write, read, other) = (&credentials[0], &credentials[1], &credentials[2]);
    let base = "/api/v3/repos/alice/proj";
    let path = format!("{base}/issues/{}", issue.number);
    document(
        request(
            &router,
            "PATCH",
            &path,
            Some(write),
            json!({"title":"Renamed","state":"closed"}),
        )
        .await,
        "issues/update",
        200,
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &format!("{base}/labels"),
            Some(write),
            json!({"name":"bug","color":"123456"}),
        )
        .await,
        "issues/create-label",
        201,
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &format!("{path}/labels"),
            Some(write),
            json!(["bug"]),
        )
        .await,
        "issues/add-labels",
        200,
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &format!("{path}/labels"),
            Some(write),
            json!(["bug"]),
        )
        .await,
        "issues/add-labels",
        200,
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &format!("{path}/assignees"),
            Some(write),
            json!({"assignees":["alice"]}),
        )
        .await,
        "issues/add-assignees",
        201,
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &format!("{path}/comments"),
            Some(write),
            json!({"body":"A comment"}),
        )
        .await,
        "issues/create-comment",
        201,
    )
    .await;
    let list = document(
        request(
            &router,
            "GET",
            &format!("{path}/events"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-events",
        200,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 4);
    assert_eq!(list[0]["event"], "renamed");
    assert_eq!(list[0]["rename"], json!({"from":"Original","to":"Renamed"}));
    assert_eq!(list[2]["label"], json!({"name":"bug","color":"123456"}));
    assert_eq!(list[3]["assignee"]["id"], alice);
    let id = list[0]["id"].as_i64().unwrap();
    let got = document(
        request(
            &router,
            "GET",
            &format!("{base}/issues/events/{id}"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/get-event",
        200,
    )
    .await;
    assert_eq!(got["issue"]["number"], issue.number);
    assert_eq!(got["event"], "renamed");
    document(
        request(
            &router,
            "GET",
            &format!("/api/v3/repos/alice/other/issues/events/{id}"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/get-event",
        404,
    )
    .await;
    let response = request(
        &router,
        "GET",
        &format!("{path}/timeline?per_page=2"),
        Some(read),
        Value::Null,
    )
    .await;
    let link = response.headers().get("link").cloned();
    let first = document(response, "issues/list-events-for-timeline", 200).await;
    assert_eq!(first.as_array().unwrap().len(), 2);
    assert!(link.unwrap().to_str().unwrap().contains("page=2"));
    let timeline = document(
        request(
            &router,
            "GET",
            &format!("{path}/timeline"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-events-for-timeline",
        200,
    )
    .await;
    assert_eq!(timeline.as_array().unwrap().len(), 5);
    assert_eq!(timeline[4]["event"], "commented");
    assert_eq!(timeline[4]["body"], "A comment");
    let repo_events = document(
        request(
            &router,
            "GET",
            &format!("{base}/issues/events"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-events-for-repo",
        200,
    )
    .await;
    assert_eq!(repo_events.as_array().unwrap().len(), 4);
    assert!(repo_events
        .as_array()
        .unwrap()
        .iter()
        .all(|event| event["issue"]["number"] == issue.number));
    for (path, operation) in [
        (format!("{path}/events"), "issues/list-events"),
        (
            format!("{path}/timeline"),
            "issues/list-events-for-timeline",
        ),
        (format!("{base}/issues/events/{id}"), "issues/get-event"),
        (
            format!("{base}/issues/events"),
            "issues/list-events-for-repo",
        ),
    ] {
        let response = request(&router, "GET", &path, Some(other), Value::Null).await;
        if matches!(
            operation,
            "issues/list-events" | "issues/list-events-for-repo"
        ) {
            assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
        } else {
            document(response, operation, 404).await;
        }
    }
    let pull = codeyard_db_repos::pull_requests::records::create_pull_request(
        f.pool(),
        codeyard_db_repos::pull_requests::records::NewPullRequest {
            issue: None,
            maintainer_can_modify: false,
            repo_id: repo.id,
            head_repo_id: Some(repo.id),
            author_id: alice,
            title: "Pull",
            body: "",
            head_branch: "topic",
            head_sha: &"a".repeat(40),
            base_branch: "main",
            base_sha: &"b".repeat(40),
            is_draft: false,
        },
    )
    .await
    .unwrap();
    codeyard_db_repos::pull_requests::reviews::add_pull_request_review(
        f.pool(),
        pull.id,
        alice,
        &codeyard_database::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(f.pool(), alice).await,
        ),
        "commented",
        Some("Published review"),
        Some(&pull.head_sha),
    )
    .await
    .unwrap();
    codeyard_db_repos::pull_requests::reviews::add_pending_pull_request_comment(
        f.pool(),
        codeyard_db_repos::pull_requests::reviews::NewPullRequestComment {
            start_line: None,
            start_side: None,
            pr_id: pull.id,
            author_id: alice,
            body: "Private draft",
            path: Some("file.txt"),
            line: Some(1),
            side: Some("RIGHT"),
            commit_sha: Some(&pull.head_sha),
            in_reply_to_comment_id: None,
            expected_revisions: Some((&pull.head_sha, &pull.base_sha)),
        },
    )
    .await
    .unwrap();
    document(
        request(
            &router,
            "POST",
            &format!("{base}/issues/{}/comments", pull.number),
            Some(write),
            json!({"body":"Pull comment"}),
        )
        .await,
        "issues/create-comment",
        201,
    )
    .await;
    let timeline = document(
        request(
            &router,
            "GET",
            &format!("{base}/issues/{}/timeline", pull.number),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-events-for-timeline",
        200,
    )
    .await;
    assert_eq!(timeline.as_array().unwrap().len(), 2);
    assert_eq!(timeline[0]["event"], "reviewed");
    assert_eq!(timeline[0]["body"], "Published review");
    assert_eq!(timeline[1]["event"], "commented");
    f.finish().await;
}
#[tokio::test]
async fn github_events_sqlite() {
    events(false).await;
}
#[tokio::test]
async fn github_events_postgres() {
    events(true).await;
}
