use super::github::{document, request};
use crate::support::{database::Database, http_client::Client};
use axum::{body::to_bytes, http::StatusCode};
use codeyard::backend::{
    db,
    identity::tokens::{self, TokenInput},
};
use serde_json::{json, Value};

async fn reactions(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let issue = db::issues::create_issue(f.pool(), repo.id, alice, "An issue", "")
        .await
        .unwrap();
    let another = db::issues::create_issue(f.pool(), repo.id, alice, "Another issue", "")
        .await
        .unwrap();
    let comment = db::issues::add_issue_comment(
        f.pool(),
        issue.repo_id,
        issue.id,
        alice,
        "A comment",
        &codeyard::backend::db::users::accounts::AccountCredential::Session(
            &crate::support::live_session(f.pool(), alice).await,
        ),
    )
    .await
    .unwrap();
    let pull = db::pull_requests::records::create_pull_request(
        f.pool(),
        db::pull_requests::records::NewPullRequest {
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
    let inline = db::pull_requests::reviews::add_pull_request_comment(
        f.pool(),
        db::pull_requests::reviews::NewPullRequestComment {
            pr_id: pull.id,
            author_id: alice,
            body: "An inline comment",
            start_line: None,
            start_side: None,
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
    let pending = db::pull_requests::reviews::add_pending_pull_request_comment(
        f.pool(),
        db::pull_requests::reviews::NewPullRequestComment {
            pr_id: pull.id,
            author_id: alice,
            body: "A pending comment",
            start_line: None,
            start_side: None,
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
    for (path, create, list, delete) in [
        (
            format!("{base}/issues/{}/reactions", issue.number),
            "reactions/create-for-issue",
            "reactions/list-for-issue",
            "reactions/delete-for-issue",
        ),
        (
            format!("{base}/issues/{}/reactions", pull.number),
            "reactions/create-for-issue",
            "reactions/list-for-issue",
            "reactions/delete-for-issue",
        ),
        (
            format!(
                "{base}/issues/comments/{}/reactions",
                codeyard::backend::webhooks::payloads::conversation_comment_id(comment.id, false)
            ),
            "reactions/create-for-issue-comment",
            "reactions/list-for-issue-comment",
            "reactions/delete-for-issue-comment",
        ),
        (
            format!("{base}/pulls/comments/{}/reactions", inline.id),
            "reactions/create-for-pull-request-review-comment",
            "reactions/list-for-pull-request-review-comment",
            "reactions/delete-for-pull-request-comment",
        ),
    ] {
        let empty = document(
            request(&router, "GET", &path, None, Value::Null).await,
            list,
            200,
        )
        .await;
        assert_eq!(empty, json!([]));
        assert_eq!(
            request(
                &router,
                "POST",
                &path,
                Some(read),
                json!({"content":"heart"})
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        document(
            request(
                &router,
                "POST",
                &path,
                Some(write),
                json!({"content":"invalid"}),
            )
            .await,
            create,
            422,
        )
        .await;
        let first = document(
            request(
                &router,
                "POST",
                &path,
                Some(write),
                json!({"content":"heart"}),
            )
            .await,
            create,
            201,
        )
        .await;
        assert_eq!(first["user"]["login"], "alice");
        let duplicate = document(
            request(
                &router,
                "POST",
                &path,
                Some(write),
                json!({"content":"heart"}),
            )
            .await,
            create,
            200,
        )
        .await;
        assert_eq!(first, duplicate);
        document(
            request(&router, "POST", &path, Some(other), json!({"content":"+1"})).await,
            create,
            201,
        )
        .await;
        let response = request(
            &router,
            "GET",
            &format!("{path}?per_page=1"),
            None,
            Value::Null,
        )
        .await;
        assert!(response.headers()["link"]
            .to_str()
            .unwrap()
            .contains("page=2"));
        let page = document(response, list, 200).await;
        assert_eq!(page, json!([first.clone()]));
        let page = document(
            request(
                &router,
                "GET",
                &format!("{path}?per_page=1&page=2"),
                None,
                Value::Null,
            )
            .await,
            list,
            200,
        )
        .await;
        assert_eq!(page[0]["user"]["login"], "bob");
        let filtered = document(
            request(
                &router,
                "GET",
                &format!("{path}?content=heart"),
                Some(read),
                Value::Null,
            )
            .await,
            list,
            200,
        )
        .await;
        assert_eq!(filtered, json!([first.clone()]));
        let remove = format!("{path}/{}", first["id"]);
        assert_eq!(
            request(&router, "DELETE", &remove, Some(other), Value::Null)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(&router, "DELETE", &remove, Some(read), Value::Null)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        let wrong_target = format!("{base}/issues/{}/reactions/{}", another.number, first["id"]);
        assert_eq!(
            request(&router, "DELETE", &wrong_target, Some(write), Value::Null)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        let response = request(&router, "DELETE", &remove, Some(write), Value::Null).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{delete}");
        assert!(to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
        let remaining = document(
            request(&router, "GET", &path, None, Value::Null).await,
            list,
            200,
        )
        .await;
        assert_eq!(remaining.as_array().unwrap().len(), 1);
    }
    let pending = format!("{base}/pulls/comments/{}/reactions", pending.id);
    for method in ["GET", "POST"] {
        assert_eq!(
            request(
                &router,
                method,
                &pending,
                Some(write),
                json!({"content":"heart"})
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
    f.execute(
        "UPDATE repositories SET is_private=true WHERE id=$1",
        &[repo.id],
    )
    .await;
    let path = format!("{base}/issues/{}/reactions", issue.number);
    for method in ["GET", "POST"] {
        assert_eq!(
            request(
                &router,
                method,
                &path,
                Some(other),
                json!({"content":"heart"})
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
    f.execute(
        "UPDATE repositories SET is_archived=true WHERE id=$1",
        &[repo.id],
    )
    .await;
    assert_eq!(
        request(
            &router,
            "POST",
            &path,
            Some(write),
            json!({"content":"heart"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    f.finish().await;
}

#[tokio::test]
async fn github_reactions_sqlite() {
    reactions(false).await;
}

#[tokio::test]
async fn github_reactions_postgres() {
    reactions(true).await;
}
