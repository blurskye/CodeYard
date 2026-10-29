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
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let credential = tokens::create(
        f.pool(),
        alice_id,
        Some(&alice.session),
        TokenInput {
            name: "stacks",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let token = format!("Bearer {}", credential.value);
    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("layer1", &base).await;
    let head1 = alice.save("layer1", "one.txt", &base).await;
    let a = alice.open_pull("One", "layer1", "main").await;
    alice.branch("layer2", &head1).await;
    let head2 = alice.save("layer2", "two.txt", &head1).await;
    let b = alice.open_pull("Two", "layer2", "layer1").await;
    alice.branch("layer3", &head2).await;
    alice.save("layer3", "three.txt", &head2).await;
    let c = alice.open_pull("Three", "layer3", "layer2").await;
    alice.branch("other", &base).await;
    alice.save("other", "other.txt", &base).await;
    let d = alice.open_pull("Unrelated", "other", "main").await;
    let (an, bn, cn, dn) = (
        a["number"].as_i64().unwrap(),
        b["number"].as_i64().unwrap(),
        c["number"].as_i64().unwrap(),
        d["number"].as_i64().unwrap(),
    );
    let url = "/api/v3/repos/alice/proj/stacks";
    // A bad later member rolls back the earlier join too.
    let (status, _, body) = exchange(
        &router,
        "pull-request-stacks/create",
        "POST",
        url,
        Some(&token),
        json!({"pull_requests":[an,bn,dn]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        db::pull_requests::records::get_pull_request(f.pool(), repo.id, bn as i32)
            .await
            .unwrap()
            .unwrap()
            .stack_id
            .is_none()
    );
    let (status, _, stack) = exchange(
        &router,
        "pull-request-stacks/create",
        "POST",
        url,
        Some(&token),
        json!({"pull_requests":[an,bn]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{stack}");
    assert_eq!(stack["base"]["ref"], "main");
    assert_eq!(stack["pull_requests"][1]["number"], bn);
    assert_eq!(stack["open"], true);
    let number = stack["number"].as_i64().unwrap();
    let at = stack["created_at"].clone();
    let stack_url = format!("{url}/{number}");
    let (status, _, body) = exchange(
        &router,
        "pull-request-stacks/get",
        "GET",
        &stack_url,
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], stack["id"]);
    let (status, _, body) = exchange(
        &router,
        "pull-request-stacks/list",
        "GET",
        &format!("{url}?pull_request={bn}&per_page=1"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    let (status, _, body) = exchange(
        &router,
        "pull-request-stacks/add",
        "POST",
        &format!("{stack_url}/add"),
        Some(&token),
        json!({"pull_requests":[cn]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["pull_requests"].as_array().unwrap().len(), 3);
    assert_eq!(body["created_at"], at);
    let (status, _, body) = exchange(
        &router,
        "pull-request-stacks/add",
        "POST",
        &format!("{stack_url}/add"),
        Some(&token),
        json!({"pull_requests":[bn]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _, body) = exchange(
        &router,
        "pull-request-stacks/unstack",
        "POST",
        &format!("{stack_url}/unstack"),
        Some(&token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _, body) = exchange(
        &router,
        "pull-request-stacks/get",
        "GET",
        &stack_url,
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    for n in [an, bn, cn] {
        assert!(
            db::pull_requests::records::get_pull_request(f.pool(), repo.id, n as i32)
                .await
                .unwrap()
                .unwrap()
                .stack_id
                .is_none()
        );
    }
    f.finish().await;
}
#[tokio::test]
async fn sqlite_github_stacks() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_stacks() {
    contract(true).await;
}
