use super::github::{document, request};
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::http::StatusCode;

use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};

async fn issues(pg: bool) {
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
    let label = codeyard_db_repos::labels::create_label(
        f.pool(),
        alice,
        repo.id,
        "bug",
        "#123456",
        Some("A bug"),
    )
    .await
    .unwrap();
    let milestone =
        codeyard_db_repos::milestones::create_milestone(f.pool(), alice, repo.id, "v1", None, None)
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
    let base = "/api/v3/repos/alice/proj/issues";
    assert_eq!(
        request(&router, "POST", base, Some(read), json!({"title":"No"}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    for input in [
        json!({}),
        json!({"title":" "}),
        json!({"title":true}),
        json!({"title":"x", "labels":["missing"]}),
        json!({"title":"x", "assignees":["bob"]}),
        json!({"title":"x", "milestone":999}),
    ] {
        let error = document(
            request(&router, "POST", base, Some(write), input).await,
            "issues/create",
            422,
        )
        .await;
        assert!(!error["errors"].as_array().unwrap().is_empty());
    }
    let created = document(request(&router, "POST", base, Some(write), json!({"title":"An issue", "body":"Details", "labels":["bug"], "assignees":["alice"], "milestone":milestone.number})).await, "issues/create", 201).await;
    assert_eq!(created["number"], 1);
    assert_eq!(created["user"]["login"], "alice");
    assert_eq!(created["assignee"]["id"], alice);
    assert_eq!(created["labels"][0]["id"], label.id);
    assert_eq!(created["milestone"]["number"], milestone.number);
    assert_eq!(created["comments"], 0);
    let path = format!("{base}/1");
    let got = document(
        request(&router, "GET", &path, Some(read), Value::Null).await,
        "issues/get",
        200,
    )
    .await;
    assert_eq!(got, created);
    for auth in [None, Some(other.as_str())] {
        document(
            request(&router, "GET", &path, auth, Value::Null).await,
            "issues/get",
            404,
        )
        .await;
    }
    document(
        request(
            &router,
            "GET",
            &format!("{base}/999"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/get",
        404,
    )
    .await;
    assert_eq!(
        request(&router, "PATCH", &path, Some(read), json!({"title":"No"}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    document(
        request(&router, "PATCH", &path, Some(write), json!({"state":"bad"})).await,
        "issues/update",
        422,
    )
    .await;
    document(
        request(
            &router,
            "PATCH",
            &path,
            Some(write),
            json!({"state_reason":"bad"}),
        )
        .await,
        "issues/update",
        422,
    )
    .await;
    document(
        request(
            &router,
            "PATCH",
            &path,
            Some(write),
            json!({"title":"Invalid edit", "milestone":999}),
        )
        .await,
        "issues/update",
        422,
    )
    .await;
    let got = document(
        request(&router, "GET", &path, Some(read), Value::Null).await,
        "issues/get",
        200,
    )
    .await;
    assert_eq!(got["title"], "An issue");
    let closed = document(request(&router, "PATCH", &path, Some(write), json!({"title":42,"body":null,"state":"closed","state_reason":"not_planned", "labels":[],"assignees":[],"milestone":null})).await, "issues/update", 200).await;
    assert_eq!(closed["title"], "42");
    assert_eq!(closed["body"], "");
    assert_eq!(closed["state"], "closed");
    assert_eq!(closed["state_reason"], "not_planned");
    assert_eq!(closed["closed_by"]["id"], alice);
    assert_eq!(closed["assignees"], json!([]));
    assert_eq!(closed["labels"], json!([]));
    assert_eq!(closed["milestone"], Value::Null);
    assert!(closed["closed_at"].is_string());
    let ignored = document(
        request(
            &router,
            "PATCH",
            &path,
            Some(write),
            json!({"state_reason":"completed"}),
        )
        .await,
        "issues/update",
        200,
    )
    .await;
    assert_eq!(ignored["state_reason"], "not_planned");
    let reopened = document(
        request(
            &router,
            "PATCH",
            &path,
            Some(write),
            json!({"state":"open"}),
        )
        .await,
        "issues/update",
        200,
    )
    .await;
    assert_eq!(reopened["state_reason"], "reopened");
    assert_eq!(reopened["closed_at"], Value::Null);
    assert_eq!(reopened["closed_by"], Value::Null);
    let assignees = format!("{path}/assignees");
    let added = document(
        request(
            &router,
            "POST",
            &assignees,
            Some(write),
            json!({"assignees":["alice","alice"]}),
        )
        .await,
        "issues/add-assignees",
        201,
    )
    .await;
    assert_eq!(added["assignees"].as_array().unwrap().len(), 1);
    let removed = document(
        request(
            &router,
            "DELETE",
            &assignees,
            Some(write),
            json!({"assignees":["alice","missing"]}),
        )
        .await,
        "issues/remove-assignees",
        200,
    )
    .await;
    assert_eq!(removed["assignees"], json!([]));
    for method in ["POST", "DELETE"] {
        assert_eq!(
            request(
                &router,
                method,
                &assignees,
                Some(read),
                json!({"assignees":["alice"]})
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        request(
            &router,
            "POST",
            &assignees,
            Some(write),
            json!({"assignees":["bob"]})
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let pull = codeyard_db_repos::pull_requests::records::create_pull_request(
        f.pool(),
        codeyard_db_repos::pull_requests::records::NewPullRequest {
            issue: None,
            maintainer_can_modify: false,
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
        },
    )
    .await
    .unwrap();
    codeyard_db_repos::pull_requests::reviews::add_pull_request_comment(
        f.pool(),
        codeyard_db_repos::pull_requests::reviews::NewPullRequestComment {
            start_line: None,
            start_side: None,
            pr_id: pull.id,
            author_id: alice,
            body: "An inline comment",
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
    let path = format!("{base}/{}", pull.number);
    let got = document(
        request(&router, "GET", &path, Some(read), Value::Null).await,
        "issues/get",
        200,
    )
    .await;
    assert_eq!(got["title"], "A pull request");
    assert!(got["pull_request"]["url"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/pulls/{}", pull.number)));
    assert_eq!(got["comments"], 0);
    let saved = document(
        request(
            &router,
            "PATCH",
            &path,
            Some(write),
            json!({"title":"Edited pull", "labels":[{"id":label.id}],"milestone":milestone.number}),
        )
        .await,
        "issues/update",
        200,
    )
    .await;
    assert_eq!(saved["title"], "Edited pull");
    assert_eq!(saved["labels"][0]["name"], "bug");
    codeyard_db_repos::assignees::assign_user(
        f.pool(),
        alice,
        repo.id,
        codeyard_db_repos::assignees::AssignedItem::PullRequest(pull.id),
        alice,
    )
    .await
    .unwrap();
    let response = request(
        &router,
        "GET",
        &format!("{base}?per_page=1"),
        Some(read),
        Value::Null,
    )
    .await;
    let link = response.headers().get("link").cloned();
    let page = document(response, "issues/list-for-repo", 200).await;
    assert_eq!(page[0]["number"], pull.number);
    assert!(link.unwrap().to_str().unwrap().contains("page=2"));
    let second = document(
        request(
            &router,
            "GET",
            &format!("{base}?page=2&per_page=1"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-for-repo",
        200,
    )
    .await;
    assert_eq!(second[0]["number"], 1);
    let ordered = document(
        request(
            &router,
            "GET",
            &format!("{base}?direction=asc&sort=created"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-for-repo",
        200,
    )
    .await;
    assert_eq!(ordered[0]["number"], 1);
    for filter in [
        "labels=bug",
        "assignee=alice",
        "assignee=*",
        "milestone=*",
        "milestone=1",
    ] {
        let filtered = document(
            request(
                &router,
                "GET",
                &format!("{base}?{filter}"),
                Some(read),
                Value::Null,
            )
            .await,
            "issues/list-for-repo",
            200,
        )
        .await;
        assert_eq!(filtered.as_array().unwrap().len(), 1, "{filter}");
        assert_eq!(filtered[0]["number"], pull.number);
    }
    for filter in ["assignee=none", "milestone=none"] {
        let filtered = document(
            request(
                &router,
                "GET",
                &format!("{base}?{filter}"),
                Some(read),
                Value::Null,
            )
            .await,
            "issues/list-for-repo",
            200,
        )
        .await;
        assert_eq!(filtered[0]["number"], 1);
    }
    for filter in [
        "creator=bob",
        "assignee=missing",
        "milestone=999",
        "state=closed",
        "since=2099-01-01T00:00:00Z",
    ] {
        let filtered = document(
            request(
                &router,
                "GET",
                &format!("{base}?{filter}"),
                Some(read),
                Value::Null,
            )
            .await,
            "issues/list-for-repo",
            200,
        )
        .await;
        assert_eq!(filtered, json!([]), "{filter}");
    }
    let updated = document(
        request(
            &router,
            "GET",
            &format!("{base}?sort=updated&direction=asc"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-for-repo",
        200,
    )
    .await;
    assert_eq!(updated.as_array().unwrap().len(), 2);
    for filter in ["state=invalid", "sort=invalid", "since=invalid"] {
        document(
            request(
                &router,
                "GET",
                &format!("{base}?{filter}"),
                Some(read),
                Value::Null,
            )
            .await,
            "issues/list-for-repo",
            422,
        )
        .await;
    }
    document(
        request(&router, "GET", base, Some(other), Value::Null).await,
        "issues/list-for-repo",
        404,
    )
    .await;
    f.execute(
        "UPDATE repositories SET is_archived=true WHERE id=$1",
        &[repo.id],
    )
    .await;
    assert_eq!(
        request(&router, "POST", base, Some(write), json!({"title":"No"}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    f.finish().await;
}

#[tokio::test]
async fn github_issues_sqlite() {
    issues(false).await;
}
#[tokio::test]
async fn github_issues_postgres() {
    issues(true).await;
}
