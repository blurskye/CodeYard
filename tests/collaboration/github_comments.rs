use super::github::{document, request};
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::{body::to_bytes, http::StatusCode};
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};

async fn comments(pg: bool) {
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
        false,
        None,
    )
    .await
    .unwrap();
    codeyard_app::repositories::lifecycle::provision(
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
    let issue = codeyard_db_repos::issues::create_issue(f.pool(), repo.id, alice, "An issue", "")
        .await
        .unwrap();
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
    let mut created = Vec::new();
    for number in [issue.number, pull.number] {
        let collection = format!("{base}/issues/{number}/comments");
        assert_eq!(
            request(
                &router,
                "POST",
                &collection,
                Some(read),
                json!({"body":"No"})
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        document(
            request(
                &router,
                "POST",
                &collection,
                Some(write),
                json!({"body":" "}),
            )
            .await,
            "issues/create-comment",
            422,
        )
        .await;
        let first = document(
            request(
                &router,
                "POST",
                &collection,
                Some(write),
                json!({"body":"First"}),
            )
            .await,
            "issues/create-comment",
            201,
        )
        .await;
        let second = document(
            request(
                &router,
                "POST",
                &collection,
                Some(other),
                json!({"body":"Second"}),
            )
            .await,
            "issues/create-comment",
            201,
        )
        .await;
        assert_ne!(first["id"], second["id"]);
        let path = format!("{base}/issues/comments/{}", first["id"]);
        let got = document(
            request(&router, "GET", &path, None, Value::Null).await,
            "issues/get-comment",
            200,
        )
        .await;
        assert_eq!(got, first);
        assert!(got["issue_url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/issues/{number}")));
        assert_eq!(
            request(&router, "PATCH", &path, Some(other), json!({"body":"No"}))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(&router, "PATCH", &path, Some(read), json!({"body":"No"}))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        let edited = document(
            request(
                &router,
                "PATCH",
                &path,
                Some(write),
                json!({"body":"Edited"}),
            )
            .await,
            "issues/update-comment",
            200,
        )
        .await;
        assert_eq!(edited["body"], "Edited");
        let response = request(
            &router,
            "GET",
            &format!("{collection}?per_page=1"),
            None,
            Value::Null,
        )
        .await;
        let link = response.headers().get("link").cloned();
        let page = document(response, "issues/list-comments", 200).await;
        assert_eq!(page[0], edited);
        assert!(link.unwrap().to_str().unwrap().contains("page=2"));
        let page = document(
            request(
                &router,
                "GET",
                &format!("{collection}?per_page=1&page=2"),
                None,
                Value::Null,
            )
            .await,
            "issues/list-comments",
            200,
        )
        .await;
        assert_eq!(page[0], second);
        let future = document(
            request(
                &router,
                "GET",
                &format!("{collection}?since=2099-01-01T00:00:00Z"),
                None,
                Value::Null,
            )
            .await,
            "issues/list-comments",
            200,
        )
        .await;
        assert_eq!(future, json!([]));
        document(
            request(
                &router,
                "POST",
                &format!("{path}/reactions"),
                Some(write),
                json!({"content":"heart"}),
            )
            .await,
            "reactions/create-for-issue-comment",
            201,
        )
        .await;
        document(
            request(
                &router,
                "GET",
                &format!("/api/v3/repos/alice/other/issues/comments/{}", first["id"]),
                None,
                Value::Null,
            )
            .await,
            "issues/get-comment",
            404,
        )
        .await;
        created.push((edited, second));
    }
    assert_ne!(created[0].0["id"], created[1].0["id"]);
    let collection = format!("{base}/issues/comments");
    let all = document(
        request(&router, "GET", &collection, Some(read), Value::Null).await,
        "issues/list-comments-for-repo",
        200,
    )
    .await;
    assert_eq!(all.as_array().unwrap().len(), 4);
    let response = request(
        &router,
        "GET",
        &format!("{collection}?sort=updated&direction=desc&per_page=1"),
        None,
        Value::Null,
    )
    .await;
    let link = response.headers().get("link").cloned();
    let page = document(response, "issues/list-comments-for-repo", 200).await;
    assert_eq!(page[0]["body"], "Edited");
    assert!(link.unwrap().to_str().unwrap().contains("page=2"));
    for (first, second) in created {
        for comment in [first, second] {
            let path = format!("{base}/issues/comments/{}", comment["id"]);
            assert_eq!(
                request(&router, "DELETE", &path, Some(read), Value::Null)
                    .await
                    .status(),
                StatusCode::FORBIDDEN
            );
            let response = request(&router, "DELETE", &path, Some(write), Value::Null).await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            assert!(to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .is_empty());
            document(
                request(&router, "GET", &path, Some(read), Value::Null).await,
                "issues/get-comment",
                404,
            )
            .await;
        }
    }
    f.execute("UPDATE issues SET locked=true WHERE id=$1", &[issue.id])
        .await;
    assert_eq!(
        request(
            &router,
            "POST",
            &format!("{base}/issues/{}/comments", issue.number),
            Some(other),
            json!({"body":"No"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    f.execute(
        "UPDATE repositories SET is_archived=true WHERE id=$1",
        &[repo.id],
    )
    .await;
    assert_eq!(
        request(
            &router,
            "POST",
            &format!("{base}/issues/{}/comments", pull.number),
            Some(write),
            json!({"body":"No"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    f.execute(
        "UPDATE repositories SET is_private=true WHERE id=$1",
        &[repo.id],
    )
    .await;
    document(
        request(&router, "GET", &collection, Some(other), Value::Null).await,
        "issues/list-comments-for-repo",
        404,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn github_comments_sqlite() {
    comments(false).await;
}
#[tokio::test]
async fn github_comments_postgres() {
    comments(true).await;
}
