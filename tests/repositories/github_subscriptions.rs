use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};

use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use tower::ServiceExt;
async fn call(
    router: &Router,
    op: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    input: Value,
    status: StatusCode,
) -> Value {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", token);
    }
    let response = router
        .clone()
        .oneshot(
            request
                .body(if input.is_null() {
                    Body::empty()
                } else {
                    Body::from(input.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let code = response.status();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        let value = serde_json::from_slice(&bytes).unwrap();
        assert_github_schema(op, code.as_u16(), &value);
        value
    };
    assert_eq!(code, status, "{op}: {value}");
    value
}
async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (name, private) in [("proj", false), ("secret", true)] {
        repos.push(
            codeyard_app::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(
                    owner_id,
                ),
                name,
                None,
                private,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), owner_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let mut tokens_out = Vec::new();
    for (id, client, scope) in [
        (owner_id, &alice, "write"),
        (bob_id, &bob, "write"),
        (owner_id, &alice, "read"),
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
        tokens_out.push(format!("Bearer {}", token.value));
    }
    let (owner, watcher, reader) = (
        tokens_out[0].as_str(),
        tokens_out[1].as_str(),
        tokens_out[2].as_str(),
    );
    let url = "/api/v3/repos/alice/proj/subscription";
    call(
        &router,
        "activity/get-repo-subscription",
        "GET",
        url,
        Some(watcher),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    call(
        &router,
        "activity/set-repo-subscription",
        "PUT",
        url,
        Some(reader),
        json!({"subscribed":true}),
        StatusCode::FORBIDDEN,
    )
    .await;
    let sub = call(
        &router,
        "activity/set-repo-subscription",
        "PUT",
        url,
        Some(watcher),
        json!({"subscribed":true}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(sub["subscribed"], true);
    assert_eq!(sub["ignored"], false);
    let at = sub["created_at"].clone();
    let repo =
        codeyard_db_repos::repositories::records::get_repository_by_id(f.pool(), repos[0].id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(repo.watchers_count, 1);
    let watchers = call(
        &router,
        "activity/list-watchers-for-repo",
        "GET",
        "/api/v3/repos/alice/proj/subscribers?per_page=1",
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(watchers[0]["login"], "bob");
    let mine = call(
        &router,
        "activity/list-watched-repos-for-authenticated-user",
        "GET",
        "/api/v3/user/subscriptions",
        Some(watcher),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(mine[0]["name"], "proj");
    let public = call(
        &router,
        "activity/list-repos-watched-by-user",
        "GET",
        "/api/v3/users/bob/subscriptions",
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(public[0]["name"], "proj");
    let sub = call(
        &router,
        "activity/set-repo-subscription",
        "PUT",
        url,
        Some(watcher),
        json!({"subscribed":false,"ignored":false}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(sub["subscribed"], false);
    assert_eq!(sub["ignored"], false);
    assert_eq!(sub["created_at"], at);
    let repo =
        codeyard_db_repos::repositories::records::get_repository_by_id(f.pool(), repos[0].id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(repo.watchers_count, 0);
    assert!(
        !codeyard_db_repos::repositories::engagement::is_repo_watched(f.pool(), bob_id, repo.id)
            .await
            .unwrap()
    );
    assert!(
        !codeyard_db_repos::repositories::engagement::list_event_watchers(
            f.pool(),
            repo.id,
            "pulls",
            0,
            100
        )
        .await
        .unwrap()
        .contains(&bob_id)
    );
    let sub = call(
        &router,
        "activity/get-repo-subscription",
        "GET",
        url,
        Some(watcher),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(sub["subscribed"], false);
    let sub = call(
        &router,
        "activity/set-repo-subscription",
        "PUT",
        url,
        Some(watcher),
        json!({"ignored":true}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(sub["ignored"], true);
    call(
        &router,
        "activity/delete-repo-subscription",
        "DELETE",
        url,
        Some(watcher),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    call(
        &router,
        "activity/get-repo-subscription",
        "GET",
        url,
        Some(watcher),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    call(
        &router,
        "activity/set-repo-subscription",
        "PUT",
        "/api/v3/repos/alice/secret/subscription",
        Some(owner),
        json!({"subscribed":true}),
        StatusCode::OK,
    )
    .await;
    let rows = call(
        &router,
        "activity/list-repos-watched-by-user",
        "GET",
        "/api/v3/users/alice/subscriptions",
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(rows.as_array().unwrap().is_empty());
    call(
        &router,
        "activity/get-repo-subscription",
        "GET",
        "/api/v3/repos/alice/secret/subscription",
        Some(watcher),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let star = "/api/v3/user/starred/alice/proj";
    call(
        &router,
        "activity/star-repo-for-authenticated-user",
        "PUT",
        star,
        Some(watcher),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    call(
        &router,
        "activity/star-repo-for-authenticated-user",
        "PUT",
        star,
        Some(watcher),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    call(
        &router,
        "activity/unstar-repo-for-authenticated-user",
        "DELETE",
        star,
        Some(watcher),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    call(
        &router,
        "activity/star-repo-for-authenticated-user",
        "PUT",
        star,
        Some(watcher),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let history = call(
        &router,
        "activity/get-stargazer-history-for-repo",
        "GET",
        "/api/v3/repos/alice/proj/stargazers/history",
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let mut total = 0;
    for week in history.as_array().unwrap() {
        assert_eq!(week["days"].as_array().unwrap().len(), 7);
        assert_eq!(week["week"].as_i64().unwrap() % 604800, 259200);
        assert_eq!(
            week["total"].as_i64().unwrap(),
            week["days"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_i64().unwrap())
                .sum::<i64>()
        );
        total += week["total"].as_i64().unwrap();
    }
    assert_eq!(
        total, 2,
        "history counts creations, not just currently retained stars"
    );
    call(
        &router,
        "activity/get-stargazer-history-for-repo",
        "GET",
        "/api/v3/repos/alice/proj/stargazers/history?page=101",
        None,
        Value::Null,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    f.finish().await;
}
#[tokio::test]
async fn sqlite_github_subscriptions() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_subscriptions() {
    contract(true).await;
}
