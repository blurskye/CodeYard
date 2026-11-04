use super::github::{document, request};
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::{body::to_bytes, http::StatusCode};
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};

async fn milestones(pg: bool) {
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
    let collection = format!("{base}/milestones");
    for token in [None, Some(other.as_str())] {
        document(
            request(&router, "GET", &collection, token, Value::Null).await,
            "issues/list-milestones",
            404,
        )
        .await;
    }
    assert_eq!(
        request(
            &router,
            "POST",
            &collection,
            Some(read),
            json!({"title":"No"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    for input in [
        json!({}),
        json!({"title":" "}),
        json!({"title":"v1", "state":"bad"}),
        json!({"title":"v1", "due_on":"2026-02-30T12:00:00Z"}),
    ] {
        let error = document(
            request(&router, "POST", &collection, Some(write), input).await,
            "issues/create-milestone",
            422,
        )
        .await;
        assert!(!error["errors"].as_array().unwrap().is_empty());
    }
    let created = document(request(&router, "POST", &collection, Some(write), json!({"title":"v1", "description":"Release one", "due_on":"2027-01-01T15:45:30+02:00"})).await, "issues/create-milestone", 201).await;
    assert_eq!(created["number"], 1);
    assert_eq!(created["creator"]["login"], "alice");
    assert_eq!(created["due_on"], "2027-01-01T13:45:30+00:00");
    assert_eq!(created["state"], "open");
    let url = format!("{collection}/1");
    let got = document(
        request(&router, "GET", &url, Some(read), Value::Null).await,
        "issues/get-milestone",
        200,
    )
    .await;
    assert_eq!(got, created);
    document(
        request(
            &router,
            "POST",
            &collection,
            Some(write),
            json!({"title":"v1"}),
        )
        .await,
        "issues/create-milestone",
        422,
    )
    .await;
    let closed = document(
        request(
            &router,
            "POST",
            &collection,
            Some(write),
            json!({"title":"v2", "state":"closed"}),
        )
        .await,
        "issues/create-milestone",
        201,
    )
    .await;
    assert_eq!(closed["state"], "closed");
    assert!(closed["closed_at"].is_string());
    let list = document(
        request(&router, "GET", &collection, Some(read), Value::Null).await,
        "issues/list-milestones",
        200,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let response = request(
        &router,
        "GET",
        &format!("{collection}?state=all&per_page=1&sort=due_on&direction=desc"),
        Some(read),
        Value::Null,
    )
    .await;
    let link = response.headers().get("link").cloned();
    let page = document(response, "issues/list-milestones", 200).await;
    assert_eq!(page[0]["number"], 1);
    assert!(link.unwrap().to_str().unwrap().contains("page=2"));
    let page = document(
        request(
            &router,
            "GET",
            &format!("{collection}?state=all&per_page=1&page=2"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-milestones",
        200,
    )
    .await;
    assert_eq!(page[0]["number"], 2);
    let edited = document(
        request(
            &router,
            "PATCH",
            &url,
            Some(write),
            json!({"title":"renamed", "state":"closed"}),
        )
        .await,
        "issues/update-milestone",
        200,
    )
    .await;
    assert_eq!(edited["due_on"], created["due_on"]);
    assert_eq!(edited["description"], created["description"]);
    assert_eq!(edited["creator"], created["creator"]);
    assert!(edited["closed_at"].is_string());
    let reopened = document(
        request(
            &router,
            "PATCH",
            &url,
            Some(write),
            json!({"due_on":null, "description":null, "state":"open"}),
        )
        .await,
        "issues/update-milestone",
        200,
    )
    .await;
    assert_eq!(reopened["due_on"], Value::Null);
    assert_eq!(reopened["description"], Value::Null);
    assert_eq!(reopened["closed_at"], Value::Null);
    for method in ["PATCH", "DELETE"] {
        assert_eq!(
            request(&router, method, &url, Some(read), json!({"title":"no"}))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let issue = codeyard_db_repos::issues::create_issue(f.pool(), repo.id, alice, "An issue", "")
        .await
        .unwrap();
    codeyard_db_repos::milestones::set_issue_milestone(f.pool(), alice, repo.id, issue.id, Some(1))
        .await
        .unwrap();
    let label =
        codeyard_db_repos::labels::create_label(f.pool(), alice, repo.id, "bug", "#123456", None)
            .await
            .unwrap();
    codeyard_db_repos::labels::add_issue_label(f.pool(), alice, repo.id, issue.id, label.id)
        .await
        .unwrap();
    let count = document(
        request(&router, "GET", &url, Some(read), Value::Null).await,
        "issues/get-milestone",
        200,
    )
    .await;
    assert_eq!(count["open_issues"], 1);
    let labels = document(
        request(
            &router,
            "GET",
            &format!("{url}/labels"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-labels-for-milestone",
        200,
    )
    .await;
    assert_eq!(labels[0]["name"], "bug");
    let response = request(&router, "DELETE", &url, Some(write), Value::Null).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(to_bytes(response.into_body(), 1024)
        .await
        .unwrap()
        .is_empty());
    assert!(
        codeyard_db_repos::issues::get_issue(f.pool(), repo.id, issue.number)
            .await
            .unwrap()
            .unwrap()
            .milestone_id
            .is_none()
    );
    document(
        request(&router, "GET", &url, Some(read), Value::Null).await,
        "issues/get-milestone",
        404,
    )
    .await;
    let native = codeyard_db_repos::milestones::create_milestone(
        f.pool(),
        alice,
        repo.id,
        "Native",
        Some("native"),
        Some("2027-03-02"),
    )
    .await
    .unwrap();
    assert_eq!(native.creator_id, Some(alice));
    assert_eq!(
        native.due_on.unwrap().to_rfc3339(),
        "2027-03-02T00:00:00+00:00"
    );
    codeyard_db_repos::milestones::update_milestone(
        f.pool(),
        alice,
        repo.id,
        native.number,
        "Updated",
        None,
        None,
    )
    .await
    .unwrap();
    let updated = codeyard_db_repos::milestones::get_milestone(f.pool(), repo.id, native.number)
        .await
        .unwrap()
        .unwrap();
    assert!(updated.milestone.updated_at >= native.updated_at);
    f.finish().await;
}

#[tokio::test]
async fn github_milestones_sqlite() {
    milestones(false).await;
}
#[tokio::test]
async fn github_milestones_postgres() {
    milestones(true).await;
}
