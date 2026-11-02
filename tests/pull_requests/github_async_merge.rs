use crate::support::{database::Database, github_spec::assert_github_schema, http_client::Client};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use codeyard::backend::{
    db,
    identity::tokens::{self, TokenInput},
    repositories::{lifecycle, pull_requests::async_merge},
};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn call(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
    expected: StatusCode,
) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", token)
                .header("content-type", "application/json")
                .body(if body.is_null() {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, expected, "{operation}: {value}");
    assert_github_schema(operation, status.as_u16(), &value);
    value
}
async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
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
    let write = tokens::create(
        f.pool(),
        alice_id,
        Some(&alice.session),
        TokenInput {
            name: "async",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let read = tokens::create(
        f.pool(),
        alice_id,
        Some(&alice.session),
        TokenInput {
            name: "reader",
            scope: "read",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let writer = format!("Bearer {}", write.value);
    let reader = format!("Bearer {}", read.value);
    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature", &base).await;
    let head = alice.save("feature", "feature.txt", &base).await;
    let pull = alice
        .open_pull("Background feature", "feature", "main")
        .await;
    let id = pull["id"].as_i64().unwrap();
    let number = pull["number"].as_i64().unwrap();
    let path = format!("/api/v3/repos/alice/proj/pulls/{number}/merge-async");
    call(
        &router,
        "pulls/merge-async",
        "PUT",
        &path,
        &reader,
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    let stale = call(
        &router,
        "pulls/merge-async",
        "PUT",
        &path,
        &writer,
        json!({"sha":"f".repeat(40)}),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(stale["status"], "failed");
    for value in [
        json!({"merge_action":"merge_queue"}),
        json!({"bypass_rules":true}),
        json!({"merge_method":"unknown"}),
    ] {
        call(
            &router,
            "pulls/merge-async",
            "PUT",
            &path,
            &writer,
            value,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    // A durable abandoned request excludes competing requests, and survives as a failed result
    // on recovery. Starting a different node must not cancel it.
    let abandoned = uuid::Uuid::new_v4().to_string();
    let details = json!({"message":"Merge request accepted","uuid":abandoned,"merge_method":"merge","merge_action":"default","expected_head_sha":head,"bypass_rules":false});
    assert!(db::pull_requests::async_merge::insert(f.pool(),db::pull_requests::async_merge::NewAsyncMerge{uuid:&abandoned,repo_id:repo.id,pull_request_id:id,token_id:write.token.id,worker_node_id:None,owner:"alice",name:"proj",number:number as i32,request:&json!({"expected_head":head,"expected_base":base,"expected_base_branch":"main"}),details:&details}).await.unwrap());
    let existing = call(
        &router,
        "pulls/get-merge-async-result",
        "GET",
        &format!("{path}/{abandoned}"),
        &writer,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(existing["status"], "pending");
    let conflict = call(
        &router,
        "pulls/merge-async",
        "PUT",
        &path,
        &writer,
        Value::Null,
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(conflict["status"], "failed");
    // A request without a worker came from a standalone server, so any node's recovery ends it.
    assert_eq!(async_merge::recover(f.pool(), Some(-99)).await.unwrap(), 1);
    assert_eq!(async_merge::recover(f.pool(), None).await.unwrap(), 0);
    let recovered = call(
        &router,
        "pulls/get-merge-async-result",
        "GET",
        &format!("{path}/{abandoned}"),
        &writer,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(recovered["status"], "failed");
    let accepted=call(&router,"pulls/merge-async","PUT",&path,&writer,json!({"sha":head,"commit_title":"Background merge","commit_message":"Accepted asynchronously"}),StatusCode::ACCEPTED).await;
    assert_eq!(accepted["status"], "pending");
    let uuid = accepted["details"]["uuid"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(uuid).is_ok());
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let result = call(
                &router,
                "pulls/get-merge-async-result",
                "GET",
                &format!("{path}/{uuid}"),
                &writer,
                Value::Null,
                StatusCode::OK,
            )
            .await;
            if result["status"] != "pending" {
                break result;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result["status"], "merged", "{result}");
    let reference = call(
        &router,
        "git/get-ref",
        "GET",
        "/api/v3/repos/alice/proj/git/ref/heads/main",
        &writer,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(reference["object"]["sha"], result["details"]["sha"]);
    assert!(
        db::pull_requests::records::get_pull_request_by_id(f.pool(), id)
            .await
            .unwrap()
            .unwrap()
            .merged
    );
    let again = call(
        &router,
        "pulls/merge-async",
        "PUT",
        &path,
        &writer,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(again["status"], "merged");
    // A request left pending after its merge was recorded is reported as that merge on recovery.
    let completed = uuid::Uuid::new_v4().to_string();
    assert!(db::pull_requests::async_merge::insert(f.pool(),db::pull_requests::async_merge::NewAsyncMerge{uuid:&completed,repo_id:repo.id,pull_request_id:id,token_id:write.token.id,worker_node_id:None,owner:"alice",name:"proj",number:number as i32,request:&json!({"expected_head":head,"expected_base":base,"expected_base_branch":"main"}),details:&details}).await.unwrap());
    assert_eq!(async_merge::recover(f.pool(), None).await.unwrap(), 1);
    let settled = db::pull_requests::async_merge::get(f.pool(), repo.id, id, &completed)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settled.status, "merged");
    assert_eq!(settled.details["sha"], result["details"]["sha"]);
    f.execute(
        "UPDATE pull_request_async_merges SET expires_at='2000-01-01T00:00:00.000000Z'",
        &[],
    )
    .await;
    call(
        &router,
        "pulls/get-merge-async-result",
        "GET",
        &format!("{path}/{uuid}"),
        &writer,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(
        db::pull_requests::async_merge::prune(f.pool())
            .await
            .unwrap(),
        3
    );
    f.finish().await;
}
#[tokio::test]
async fn sqlite_github_async_merge() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_async_merge() {
    contract(true).await;
}
