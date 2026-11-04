//! GitHub labels through the real router on SQLite and PostgreSQL.
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    response::Response,
    Router,
};
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_app::ci::config::CiConfig;
use codeyard_app::storage::ObjectStore;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

pub(super) async fn request(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> Response {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        req = req.header("authorization", token);
    }
    router
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

pub(super) async fn document(response: Response, operation: &str, expected: u16) -> Value {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{operation}: {status} without a JSON body: {error}"));
    assert_eq!(status.as_u16(), expected, "{operation}: {body}");
    assert_github_schema(operation, expected, &body);
    body
}

async fn labels(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_app::repositories::lifecycle::provision(
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
    let issue = codeyard_db_repos::issues::create_issue(f.pool(), repo.id, alice, "An issue", "")
        .await
        .unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
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
    let labels = format!("{base}/labels");
    let on_issue = format!("{base}/issues/{}/labels", issue.number);
    for (method, path, body) in [
        ("POST", labels.as_str(), json!({"name":"blocked"})),
        ("POST", on_issue.as_str(), json!({"labels":[]})),
        ("PUT", on_issue.as_str(), json!({"labels":[]})),
        ("DELETE", on_issue.as_str(), Value::Null),
    ] {
        assert_eq!(
            request(&router, method, path, Some(read), body)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    for token in [None, Some(other.as_str())] {
        document(
            request(&router, "GET", &labels, token, Value::Null).await,
            "issues/list-labels-for-repo",
            404,
        )
        .await;
    }
    for input in [
        json!({"name":""}),
        json!({"name":"bug", "color":"#ff0000"}),
        json!({"name":"bug", "color":"oops"}),
        json!({"name":"a\u{0000}b"}),
    ] {
        let body = document(
            request(&router, "POST", &labels, Some(write), input).await,
            "issues/create-label",
            422,
        )
        .await;
        assert!(body["errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty()));
    }
    let bug = document(
        request(
            &router,
            "POST",
            &labels,
            Some(write),
            json!({"name":"bug", "color":"FF0000", "description":"A bug"}),
        )
        .await,
        "issues/create-label",
        201,
    )
    .await;
    assert_eq!(bug["color"], "ff0000");
    assert!(bug["url"]
        .as_str()
        .unwrap()
        .ends_with("/api/v3/repos/alice/proj/labels/bug"));
    // gh label create --force falls back to an update only on GitHub's already_exists item.
    let taken = document(
        request(&router, "POST", &labels, Some(write), json!({"name":"bug"})).await,
        "issues/create-label",
        422,
    )
    .await;
    assert_eq!(
        taken["errors"],
        json!([{"resource":"Label","field":"name","code":"already_exists"}])
    );
    document(
        request(
            &router,
            "POST",
            &labels,
            Some(write),
            json!({"name":"triage"}),
        )
        .await,
        "issues/create-label",
        201,
    )
    .await;
    let response = request(
        &router,
        "GET",
        &format!("{labels}?per_page=1"),
        Some(read),
        Value::Null,
    )
    .await;
    assert!(response.headers()["link"]
        .to_str()
        .unwrap()
        .contains("page=2"));
    let page = document(response, "issues/list-labels-for-repo", 200).await;
    assert_eq!(page.as_array().unwrap().len(), 1);
    let page = document(
        request(
            &router,
            "GET",
            &format!("{labels}?per_page=1&page=2"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-labels-for-repo",
        200,
    )
    .await;
    assert_eq!(page[0]["name"], "triage");
    document(
        request(
            &router,
            "GET",
            &format!("{labels}/BUG"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/get-label",
        200,
    )
    .await;
    document(
        request(
            &router,
            "GET",
            &format!("{labels}/missing"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/get-label",
        404,
    )
    .await;
    let changed = document(
        request(
            &router,
            "PATCH",
            &format!("{labels}/bug"),
            Some(write),
            json!({"new_name":"bug fix", "description":"", "color":"abcdef"}),
        )
        .await,
        "issues/update-label",
        200,
    )
    .await;
    assert_eq!(changed["id"], bug["id"]);
    assert_eq!(changed["color"], "abcdef");
    assert_eq!(changed["description"], "");
    assert!(changed["url"]
        .as_str()
        .unwrap()
        .ends_with("/labels/bug%20fix"));
    // A left-out description is kept, and an explicit null clears it.
    for (body, expected) in [
        (json!({"description":"notes"}), json!("notes")),
        (json!({"color":"123456"}), json!("notes")),
        (json!({"description":null}), Value::Null),
    ] {
        let changed = document(
            request(
                &router,
                "PATCH",
                &format!("{labels}/bug%20fix"),
                Some(write),
                body,
            )
            .await,
            "issues/update-label",
            200,
        )
        .await;
        assert_eq!(changed["description"], expected);
    }
    for method in ["PATCH", "DELETE"] {
        assert_eq!(
            request(
                &router,
                method,
                &format!("{labels}/bug%20fix"),
                Some(read),
                json!({"new_name":"no"})
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    let added = document(
        request(
            &router,
            "POST",
            &on_issue,
            Some(write),
            json!({"labels":["bug fix", "bug fix", "triage"]}),
        )
        .await,
        "issues/add-labels",
        200,
    )
    .await;
    assert_eq!(added.as_array().unwrap().len(), 2);
    document(
        request(&router, "POST", &on_issue, Some(write), json!(["missing"])).await,
        "issues/add-labels",
        422,
    )
    .await;
    document(
        request(
            &router,
            "PUT",
            &on_issue,
            Some(write),
            json!(["triage", "missing"]),
        )
        .await,
        "issues/set-labels",
        422,
    )
    .await;
    let current = document(
        request(&router, "GET", &on_issue, Some(read), Value::Null).await,
        "issues/list-labels-on-issue",
        200,
    )
    .await;
    assert_eq!(current, added);
    let set = document(
        request(&router, "PUT", &on_issue, Some(write), json!(["triage"])).await,
        "issues/set-labels",
        200,
    )
    .await;
    assert_eq!(set.as_array().unwrap().len(), 1);
    let removed = document(
        request(
            &router,
            "DELETE",
            &format!("{on_issue}/triage"),
            Some(write),
            Value::Null,
        )
        .await,
        "issues/remove-label",
        200,
    )
    .await;
    assert_eq!(removed, json!([]));
    document(
        request(
            &router,
            "DELETE",
            &format!("{on_issue}/triage"),
            Some(write),
            Value::Null,
        )
        .await,
        "issues/remove-label",
        404,
    )
    .await;
    document(
        request(&router, "POST", &on_issue, Some(write), json!(["triage"])).await,
        "issues/add-labels",
        200,
    )
    .await;
    let pull = codeyard_db_repos::pull_requests::records::create_pull_request(
        f.pool(),
        codeyard_db_repos::pull_requests::records::NewPullRequest {
            repo_id: repo.id,
            head_repo_id: Some(repo.id),
            author_id: alice,
            title: "A pull request",
            body: "",
            head_branch: "topic",
            head_sha: &"a".repeat(40),
            base_branch: "main",
            base_sha: &"b".repeat(40),
            is_draft: false,
            maintainer_can_modify: false,
            issue: None,
        },
    )
    .await
    .unwrap();
    let on_pull = format!("{base}/issues/{}/labels", pull.number);
    document(
        request(
            &router,
            "POST",
            &on_pull,
            Some(write),
            json!(["triage", "bug fix"]),
        )
        .await,
        "issues/add-labels",
        200,
    )
    .await;
    let replaced = document(
        request(
            &router,
            "PUT",
            &on_pull,
            Some(write),
            json!({"labels":["triage"]}),
        )
        .await,
        "issues/set-labels",
        200,
    )
    .await;
    assert_eq!(replaced.as_array().unwrap().len(), 1);
    let replaced = document(
        request(&router, "GET", &on_pull, Some(read), Value::Null).await,
        "issues/list-labels-on-issue",
        200,
    )
    .await;
    assert_eq!(replaced[0]["name"], "triage");
    document(
        request(
            &router,
            "DELETE",
            &format!("{on_pull}/triage"),
            Some(write),
            Value::Null,
        )
        .await,
        "issues/remove-label",
        200,
    )
    .await;
    let response = request(&router, "DELETE", &on_pull, Some(write), Value::Null).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = request(&router, "DELETE", &on_issue, Some(write), Value::Null).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(to_bytes(response.into_body(), 1024)
        .await
        .unwrap()
        .is_empty());
    let removed = document(
        request(&router, "GET", &on_issue, Some(read), Value::Null).await,
        "issues/list-labels-on-issue",
        200,
    )
    .await;
    assert_eq!(removed, json!([]));
    document(
        request(&router, "POST", &on_issue, Some(write), json!(["triage"])).await,
        "issues/add-labels",
        200,
    )
    .await;
    let response = request(
        &router,
        "DELETE",
        &format!("{labels}/triage"),
        Some(write),
        Value::Null,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(to_bytes(response.into_body(), 1024)
        .await
        .unwrap()
        .is_empty());
    let removed = document(
        request(&router, "GET", &on_issue, Some(read), Value::Null).await,
        "issues/list-labels-on-issue",
        200,
    )
    .await;
    assert_eq!(removed, json!([]));
    document(
        request(
            &router,
            "GET",
            &format!("{base}/issues/999/labels"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-labels-on-issue",
        404,
    )
    .await;
    let assignees = document(
        request(
            &router,
            "GET",
            &format!("{base}/assignees?per_page=1"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-assignees",
        200,
    )
    .await;
    assert_eq!(assignees[0]["login"], "alice");
    for path in [
        format!("{base}/assignees/alice"),
        format!("{base}/issues/{}/assignees/alice", issue.number),
        format!("{base}/issues/{}/assignees/alice", pull.number),
    ] {
        let response = request(&router, "GET", &path, Some(read), Value::Null).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
    }
    for (path, operation) in [
        (
            format!("{base}/assignees/bob"),
            "issues/check-user-can-be-assigned",
        ),
        (
            format!("{base}/issues/{}/assignees/bob", issue.number),
            "issues/check-user-can-be-assigned-to-issue",
        ),
        (
            format!("{base}/issues/999/assignees/alice"),
            "issues/check-user-can-be-assigned-to-issue",
        ),
    ] {
        document(
            request(&router, "GET", &path, Some(read), Value::Null).await,
            operation,
            404,
        )
        .await;
    }
    document(
        request(
            &router,
            "GET",
            &format!("{base}/assignees"),
            Some(other),
            Value::Null,
        )
        .await,
        "issues/list-assignees",
        404,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn github_labels_sqlite() {
    labels(false).await;
}

#[tokio::test]
async fn github_labels_postgres() {
    labels(true).await;
}
