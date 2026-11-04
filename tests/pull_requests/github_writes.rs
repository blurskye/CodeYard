use super::github::exchange;
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::{http::StatusCode, Router};
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};

async fn call(
    router: &Router,
    operation: &str,
    method: &str,
    url: &str,
    token: Option<&str>,
    body: Value,
    status: StatusCode,
) -> Value {
    let (code, _, body) = exchange(router, operation, method, url, token, body).await;
    assert_eq!(code, status, "{operation}: {body}");
    body
}
async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob_id,
        repo.id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let mut credentials = Vec::new();
    for (id, client, scope) in [
        (alice_id, &alice, "write"),
        (bob_id, &bob, "write"),
        (alice_id, &alice, "read"),
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
        credentials.push(format!("Bearer {}", token.value));
    }
    let (owner, reviewer, reader) = (
        credentials[0].as_str(),
        credentials[1].as_str(),
        credentials[2].as_str(),
    );
    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature", &base).await;
    let head = alice.save("feature", "feature.txt", &base).await;
    let root_url = "/api/v3/repos/alice/proj";
    let input = json!({"title":"Feature","head":"feature","base":"main","body":"Opening body","draft":true,"maintainer_can_modify":false});
    call(
        &router,
        "pulls/create",
        "POST",
        &format!("{root_url}/pulls"),
        Some(reader),
        input.clone(),
        StatusCode::FORBIDDEN,
    )
    .await;
    let pull = call(
        &router,
        "pulls/create",
        "POST",
        &format!("{root_url}/pulls"),
        Some(owner),
        input.clone(),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(pull["title"], "Feature");
    assert_eq!(pull["draft"], true);
    assert_eq!(pull["head"]["sha"], head);
    call(
        &router,
        "pulls/create",
        "POST",
        &format!("{root_url}/pulls"),
        Some(owner),
        input,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let number = pull["number"].as_i64().unwrap();
    let url = format!("{root_url}/pulls/{number}");
    for state in ["closed", "open"] {
        let updated = call(
            &router,
            "pulls/update",
            "PATCH",
            &url,
            Some(owner),
            json!({"title":"Edited feature","body":"Edited body","state":state}),
            StatusCode::OK,
        )
        .await;
        assert_eq!(updated["state"], state);
        assert_eq!(updated["body"], "Edited body");
    }
    call(
        &router,
        "pulls/update",
        "PATCH",
        &url,
        Some(owner),
        json!({"base":"feature"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    call(
        &router,
        "pulls/update",
        "PATCH",
        &url,
        Some(reader),
        json!({"title":"Denied"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    // Ready the draft through the native domain before asking for reviews.
    let (status, body) = alice
        .api(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/ready"),
            json!({}),
        )
        .await;
    assert!(status.is_success(), "{status}: {body}");
    let requested = call(
        &router,
        "pulls/request-reviewers",
        "POST",
        &format!("{url}/requested_reviewers"),
        Some(owner),
        json!({"reviewers":["bob"]}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(requested["requested_reviewers"][0]["login"], "bob");
    let comment = call(
        &router,
        "pulls/create-review-comment",
        "POST",
        &format!("{url}/comments"),
        Some(reviewer),
        json!({"body":"Inline","path":"feature.txt","line":1,"side":"RIGHT","commit_id":head}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(comment["line"], 1);
    assert_eq!(comment["side"], "RIGHT");
    assert!(!comment["diff_hunk"].as_str().unwrap().is_empty());
    let id = comment["id"].as_i64().unwrap();
    let comment_url = format!("{root_url}/pulls/comments/{id}");
    call(
        &router,
        "pulls/get-review-comment",
        "GET",
        &comment_url,
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let updated = call(
        &router,
        "pulls/update-review-comment",
        "PATCH",
        &comment_url,
        Some(reviewer),
        json!({"body":"Edited inline"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(updated["body"], "Edited inline");
    call(
        &router,
        "pulls/create-review-comment",
        "POST",
        &format!("{url}/comments"),
        Some(reviewer),
        json!({"body":"Bad anchor","path":"feature.txt","line":999,"commit_id":head}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let reply = call(
        &router,
        "pulls/create-reply-for-review-comment",
        "POST",
        &format!("{url}/comments/{id}/replies"),
        Some(owner),
        json!({"body":"Reply"}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(reply["in_reply_to_id"], id);
    let reply_id = reply["id"].as_i64().unwrap();
    for (operation, path) in [
        (
            "pulls/list-review-comments",
            format!("{url}/comments?per_page=1"),
        ),
        (
            "pulls/list-review-comments-for-repo",
            format!("{root_url}/pulls/comments?sort=updated&direction=desc"),
        ),
    ] {
        let rows = call(
            &router,
            operation,
            "GET",
            &path,
            None,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert!(!rows.as_array().unwrap().is_empty());
    }
    call(
        &router,
        "pulls/delete-review-comment",
        "DELETE",
        &format!("{root_url}/pulls/comments/{reply_id}"),
        Some(owner),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    call(
        &router,
        "pulls/delete-review-comment",
        "DELETE",
        &comment_url,
        Some(reviewer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    call(
        &router,
        "pulls/get-review-comment",
        "GET",
        &comment_url,
        None,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let draft=call(&router,"pulls/create-review","POST",&format!("{url}/reviews"),Some(reviewer),json!({"body":"Draft summary","comments":[{"path":"feature.txt","body":"Private inline","line":1,"side":"RIGHT"}]}),StatusCode::OK).await;
    assert_eq!(draft["state"], "PENDING");
    assert!(draft.get("submitted_at").is_none());
    let review_id = draft["id"].as_i64().unwrap();
    let review_url = format!("{url}/reviews/{review_id}");
    let comments = call(
        &router,
        "pulls/list-comments-for-review",
        "GET",
        &format!("{review_url}/comments"),
        Some(reviewer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(comments.as_array().unwrap().len(), 1);
    let private_comment = comments[0]["id"].as_i64().unwrap();
    call(
        &router,
        "pulls/get-review-comment",
        "GET",
        &format!("{root_url}/pulls/comments/{private_comment}"),
        Some(owner),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let updated = call(
        &router,
        "pulls/update-review",
        "PUT",
        &review_url,
        Some(reviewer),
        json!({"body":"Updated summary"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(updated["body"], "Updated summary");
    call(
        &router,
        "pulls/submit-review",
        "POST",
        &format!("{review_url}/events"),
        Some(reviewer),
        json!({"event":"BOGUS"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let submitted = call(
        &router,
        "pulls/submit-review",
        "POST",
        &format!("{review_url}/events"),
        Some(reviewer),
        json!({"event":"APPROVE"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(submitted["id"], review_id);
    assert_eq!(submitted["state"], "APPROVED");
    assert_eq!(submitted["body"], "Updated summary");
    call(
        &router,
        "pulls/get-review-comment",
        "GET",
        &format!("{root_url}/pulls/comments/{private_comment}"),
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    call(
        &router,
        "pulls/update-review-comment",
        "PATCH",
        &format!("{root_url}/pulls/comments/{private_comment}"),
        Some(reviewer),
        json!({"body":"Published edit"}),
        StatusCode::OK,
    )
    .await;
    call(
        &router,
        "pulls/delete-pending-review",
        "DELETE",
        &review_url,
        Some(reviewer),
        Value::Null,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let draft = call(
        &router,
        "pulls/create-review",
        "POST",
        &format!("{url}/reviews"),
        Some(reviewer),
        json!({"body":"Discard me"}),
        StatusCode::OK,
    )
    .await;
    let draft_url = format!("{url}/reviews/{}", draft["id"]);
    let deleted = call(
        &router,
        "pulls/delete-pending-review",
        "DELETE",
        &draft_url,
        Some(reviewer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(deleted["id"], draft["id"]);
    call(
        &router,
        "pulls/request-reviewers",
        "POST",
        &format!("{url}/requested_reviewers"),
        Some(owner),
        json!({"reviewers":["bob"]}),
        StatusCode::CREATED,
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
    .into_iter()
    .filter(|n| n.kind == "pull_request_review_requested")
    .count();
    call(
        &router,
        "pulls/rerequest-reviewers",
        "POST",
        &format!("{url}/requested_reviewers/rerequest"),
        Some(owner),
        json!({"reviewers":["bob"]}),
        StatusCode::CREATED,
    )
    .await;
    let after = codeyard_db_repos::notifications::records::list_notifications_for_user(
        f.pool(),
        bob_id,
        100,
        false,
    )
    .await
    .unwrap()
    .into_iter()
    .filter(|n| n.kind == "pull_request_review_requested")
    .count();
    assert_eq!(after, before + 1);
    let removed = call(
        &router,
        "pulls/remove-requested-reviewers",
        "DELETE",
        &format!("{url}/requested_reviewers"),
        Some(owner),
        json!({"reviewers":["bob"]}),
        StatusCode::OK,
    )
    .await;
    assert!(removed["requested_reviewers"]
        .as_array()
        .unwrap()
        .is_empty());
    f.finish().await;
}
#[tokio::test]
async fn sqlite_github_writes() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_writes() {
    contract(true).await;
}
