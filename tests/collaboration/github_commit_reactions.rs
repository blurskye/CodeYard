use super::github::{document, request};
use crate::support::{database::Database, http_client::Client};
use axum::{body::to_bytes, http::StatusCode};
use codeyard::backend::{
    db,
    identity::tokens::{self, TokenInput},
};
use serde_json::{json, Value};
async fn reactions(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "pulls",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), alice).await;
    let outsider = Client::login(&router, f.pool(), bob).await;
    browser.expect("POST","/api/repos/alice/pulls/contents/README.md",json!({"content":"SGVsbG8K","message":"Initial content","branch":"main","expected_revision":"unborn"}),StatusCode::CREATED).await;
    let mut credentials = Vec::new();
    for (id, scope, session) in [
        (alice, "write", browser.session.as_str()),
        (alice, "read", browser.session.as_str()),
        (bob, "write", outsider.session.as_str()),
    ] {
        let token = tokens::create(
            f.pool(),
            id,
            Some(session),
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
    let base = "/api/v3/repos/alice/pulls";
    let comment = document(
        request(
            &router,
            "POST",
            &format!("{base}/commits/main/comments"),
            Some(write),
            json!({"body":"Commit discussion"}),
        )
        .await,
        "repos/create-commit-comment",
        201,
    )
    .await;
    let id = comment["id"].as_i64().unwrap();
    let sha = comment["commit_id"].as_str().unwrap();
    let path = format!("{base}/comments/{id}/reactions");
    let one = document(
        request(
            &router,
            "POST",
            &path,
            Some(write),
            json!({"content":"heart"}),
        )
        .await,
        "reactions/create-for-commit-comment",
        201,
    )
    .await;
    let same = document(
        request(
            &router,
            "POST",
            &path,
            Some(write),
            json!({"content":"heart"}),
        )
        .await,
        "reactions/create-for-commit-comment",
        200,
    )
    .await;
    assert_eq!(one["id"], same["id"]);
    let two = document(
        request(
            &router,
            "POST",
            &path,
            Some(other),
            json!({"content":"eyes"}),
        )
        .await,
        "reactions/create-for-commit-comment",
        201,
    )
    .await;
    let first = request(
        &router,
        "GET",
        &format!("{path}?per_page=1"),
        None,
        Value::Null,
    )
    .await;
    assert!(first.headers()["link"].to_str().unwrap().contains("page=2"));
    let first = document(first, "reactions/list-for-commit-comment", 200).await;
    assert_eq!(first.as_array().unwrap().len(), 1);
    assert_eq!(first[0]["id"], one["id"]);
    let filtered = document(
        request(
            &router,
            "GET",
            &format!("{path}?content=eyes"),
            Some(read),
            Value::Null,
        )
        .await,
        "reactions/list-for-commit-comment",
        200,
    )
    .await;
    assert_eq!(filtered[0]["id"], two["id"]);
    document(
        request(
            &router,
            "POST",
            &path,
            Some(write),
            json!({"content":"unknown"}),
        )
        .await,
        "reactions/create-for-commit-comment",
        422,
    )
    .await;
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
            "DELETE",
            &format!("{path}/{}", two["id"]),
            Some(write),
            Value::Null,
        )
        .await,
        "reactions/delete-for-commit-comment",
        404,
    )
    .await;
    let native = format!("/alice/pulls/commit/{sha}/comments/{id}/react?view=split");
    let reply = browser.form_reply(&native, &[("content", "laugh")]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert!(reply
        .location
        .unwrap()
        .contains("&view=split#commitcomment-"));
    let (status, model) = browser.model(&format!("/alice/pulls/commit/{sha}")).await;
    assert_eq!(status, StatusCode::OK);
    let codeyard::webui::app::Page::Repositories(
        codeyard::webui::pages::repos::page::Page::CommitDetail { discussion, .. },
    ) = model
    else {
        panic!("commit discussion")
    };
    assert!(discussion
        .comments
        .iter()
        .find(|c| c.id == id)
        .unwrap()
        .reactions
        .iter()
        .any(|r| r.content == "laugh" && r.mine && r.count == 1));
    let got = document(
        request(
            &router,
            "GET",
            &format!("{base}/comments/{id}"),
            Some(read),
            Value::Null,
        )
        .await,
        "repos/get-commit-comment",
        200,
    )
    .await;
    assert_eq!(got["reactions"]["total_count"], 3);
    assert_eq!(got["reactions"]["laugh"], 1);
    let list = document(
        request(
            &router,
            "GET",
            &format!("{base}/comments"),
            Some(read),
            Value::Null,
        )
        .await,
        "repos/list-commit-comments-for-repo",
        200,
    )
    .await;
    assert_eq!(list[0]["reactions"]["total_count"], 3);
    assert_eq!(
        browser
            .form_reply(&native, &[("content", "laugh")])
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    let csrf = browser
        .exchange(
            "POST",
            &native,
            "application/x-www-form-urlencoded",
            b"content=rocket".to_vec(),
        )
        .await;
    assert_eq!(csrf.status, StatusCode::FORBIDDEN);
    let wrong_commit = native.replace(sha, &"0".repeat(40));
    assert_eq!(
        browser
            .form_reply(&wrong_commit, &[("content", "rocket")])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let removed = request(
        &router,
        "DELETE",
        &format!("{path}/{}", one["id"]),
        Some(write),
        Value::Null,
    )
    .await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert!(to_bytes(removed.into_body(), 1024)
        .await
        .unwrap()
        .is_empty());
    f.execute(
        "UPDATE repositories SET is_private=true WHERE id=$1",
        &[repo.id],
    )
    .await;
    document(
        request(&router, "GET", &path, Some(other), Value::Null).await,
        "reactions/list-for-commit-comment",
        404,
    )
    .await;
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
            json!({"content":"rocket"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    f.execute(
        "UPDATE repositories SET is_archived=false WHERE id=$1",
        &[repo.id],
    )
    .await;
    let issue = db::issues::create_issue(f.pool(), repo.id, alice, "Independent reaction", "")
        .await
        .unwrap();
    let issue_comment = db::issues::add_issue_comment(f.pool(), issue.repo_id, issue.id, alice, "Comment", &codeyard::backend::db::users::accounts::AccountCredential::Session(&crate::support::live_session(f.pool(), alice).await))
        .await
        .unwrap();
    db::reactions::create_reaction(
        f.pool(),
        alice,
        &db::users::accounts::AccountCredential::Session(&browser.session),
        db::reactions::Target {
            repo_id: repo.id,
            item_id: issue.id,
            subject: db::reactions::Subject::IssueComment,
            subject_id: issue_comment.id,
        },
        "heart",
    )
    .await
    .unwrap();
    let deleted = request(
        &router,
        "DELETE",
        &format!("{base}/comments/{id}"),
        Some(write),
        Value::Null,
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(
        db::reactions::reaction_counts(f.pool(), "commit_comment", &[id], alice)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db::reactions::reaction_counts(f.pool(), "issue_comment", &[issue_comment.id], alice)
            .await
            .unwrap()[0]
            .count,
        1
    );
    f.finish().await;
}
#[tokio::test]
async fn github_commit_reactions_sqlite() {
    reactions(false).await;
}
#[tokio::test]
async fn github_commit_reactions_postgres() {
    reactions(true).await;
}
