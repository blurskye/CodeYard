//! Review reads retain pending-review privacy, and dismissals use live administration rights.
use super::github::exchange;
use crate::support::{database::Database, http_client::Client};
use axum::http::StatusCode;
use codeyard::backend::{
    db,
    identity::tokens::{self, TokenInput},
};
use serde_json::{json, Value};

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
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
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob_id,
        repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let mut credentials = Vec::new();
    for (id, client) in [(alice_id, &alice), (bob_id, &bob)] {
        let token = tokens::create(
            f.pool(),
            id,
            Some(&client.session),
            TokenInput {
                name: "reviews",
                scope: "write",
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let owner = credentials[0].as_str();
    let reviewer = credentials[1].as_str();
    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature", &base).await;
    let head = alice.save("feature", "feature.txt", &base).await;
    let pull = alice.open_pull("Feature", "feature", "main").await;
    let id = pull["id"].as_i64().unwrap();
    let number = pull["number"].as_i64().unwrap();
    let old = format!("/api/repos/alice/proj/pulls/{number}");
    let url = format!("/api/v3/repos/alice/proj/pulls/{number}");
    let (status, body) = alice
        .api(
            "POST",
            &format!("{old}/requested_reviewers"),
            json!({"username": "bob"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _, body) = exchange(
        &router,
        "pulls/list-requested-reviewers",
        "GET",
        &format!("{url}/requested_reviewers"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["users"][0]["login"], "bob");
    assert!(body["teams"].as_array().unwrap().is_empty());
    let (status, review) = bob
        .api(
            "POST",
            &format!("{old}/reviews"),
            json!({"state": "approved", "body": "Looks good", "commit_sha": head}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{review}");
    let review_id = review["id"].as_i64().unwrap();
    let (status, _, body) = exchange(
        &router,
        "pulls/get-review",
        "GET",
        &format!("{url}/reviews/{review_id}"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "APPROVED");
    assert_eq!(body["body"], "Looks good");
    assert_eq!(body["commit_id"], head);
    assert_eq!(body["author_association"], "COLLABORATOR");
    let (_, _, body) = exchange(
        &router,
        "pulls/list-requested-reviewers",
        "GET",
        &format!("{url}/requested_reviewers"),
        None,
        Value::Null,
    )
    .await;
    assert!(body["users"].as_array().unwrap().is_empty());

    let (status, comment) = bob.api("POST", &format!("{old}/comments"), json!({"body": "Still thinking", "path": "feature.txt", "line": 1, "side": "RIGHT", "commit_sha": head, "base_sha": base, "pending": true})).await;
    assert_eq!(status, StatusCode::CREATED, "{comment}");
    let pending = db::pull_requests::reviews::get_pending_review(f.pool(), id, bob_id)
        .await
        .unwrap()
        .unwrap();
    let (status, _, body) = exchange(
        &router,
        "pulls/get-review",
        "GET",
        &format!("{url}/reviews/{}", pending.id),
        Some(reviewer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "PENDING");
    assert!(body.get("submitted_at").is_none());
    for token in [None, Some(owner)] {
        let (status, _, _) = exchange(
            &router,
            "pulls/get-review",
            "GET",
            &format!("{url}/reviews/{}", pending.id),
            token,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (status, comment) = alice
        .api(
            "POST",
            &format!("{old}/reviews"),
            json!({"state": "commented", "body": "Thanks for looking", "commit_sha": head}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{comment}");
    let (_, _, body) = exchange(
        &router,
        "pulls/list-reviews",
        "GET",
        &format!("{url}/reviews"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 2);
    assert!(body
        .as_array()
        .unwrap()
        .iter()
        .all(|review| review["state"] != "PENDING"));
    let (_, headers, body) = exchange(
        &router,
        "pulls/list-reviews",
        "GET",
        &format!("{url}/reviews?per_page=2"),
        Some(reviewer),
        Value::Null,
    )
    .await;
    assert_eq!(body[0]["id"], review_id);
    assert_eq!(body[1]["id"], pending.id);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (_, headers, body) = exchange(
        &router,
        "pulls/list-reviews",
        "GET",
        &format!("{url}/reviews?per_page=2&page=2"),
        Some(reviewer),
        Value::Null,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["state"], "COMMENTED");
    assert!(!headers["link"].to_str().unwrap().contains("rel=\"next\""));

    let dismissal = format!("{url}/reviews/{review_id}/dismissals");
    let (status, _, _) = exchange(
        &router,
        "pulls/dismiss-review",
        "PUT",
        &dismissal,
        Some(reviewer),
        json!({"message": "I cannot dismiss votes"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for input in [
        json!({}),
        json!({"message": ""}),
        json!({"message": "Reason", "event": "APPROVE"}),
    ] {
        let (status, _, body) = exchange(
            &router,
            "pulls/dismiss-review",
            "PUT",
            &dismissal,
            Some(owner),
            input,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(!body["errors"].as_array().unwrap().is_empty());
    }
    let (status, _, body) = exchange(
        &router,
        "pulls/dismiss-review",
        "PUT",
        &dismissal,
        Some(owner),
        json!({"message": "Approval no longer applies", "event": "DISMISS"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "DISMISSED");
    let review = db::pull_requests::reviews::get_pull_request_review_by_id(f.pool(), review_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(review.dismissed_by_id, Some(alice_id));
    assert_eq!(
        review.dismissed_reason.as_deref(),
        Some("Approval no longer applies")
    );
    let (status, _, _) = exchange(
        &router,
        "pulls/get-review",
        "GET",
        "/api/v3/repos/alice/proj/pulls/999/reviews/1",
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_github_reviews() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_reviews() {
    contract(true).await;
}
