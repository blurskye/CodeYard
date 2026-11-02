//! Lookup fields answer with GitHub's NOT_FOUND wording or null, which gh matches on.
use super::{call, json, query, request};
use axum::{http::StatusCode, Router};
use serde_json::Value;

async fn refused(router: &Router, token: &str, document: &str) -> Value {
    let (status, body) = call(router, Some(token), document, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["errors"][0]["type"], "NOT_FOUND", "{body}");
    body
}

pub(super) async fn exercise(router: &Router, token: &str) {
    let body = refused(
        router,
        token,
        "{repository(owner:\"graphql-alice\",name:\"missing\"){id}}",
    )
    .await;
    assert_eq!(
        body["errors"][0]["message"],
        "Could not resolve to a Repository with the name 'graphql-alice/missing'."
    );
    let body = refused(router, token, "{organization(login:\"graphql-alice\"){id}}").await;
    assert_eq!(
        body["errors"][0]["message"],
        "Could not resolve to an Organization with the login of 'graphql-alice'."
    );
    let body = refused(router, token, "{user(login:\"graphql-nobody\"){id}}").await;
    assert_eq!(
        body["errors"][0]["message"],
        "Could not resolve to a User with the login of 'graphql-nobody'."
    );
    let body = refused(
        router,
        token,
        "{repository(owner:\"graphql-alice\",name:\"proj\"){pullRequest(number:999999){id}}}",
    )
    .await;
    assert_eq!(
        body["errors"][0]["message"],
        "Could not resolve to a PullRequest with the number of 999999."
    );
    assert_eq!(body["data"]["repository"]["pullRequest"], Value::Null);
    let body = refused(
        router,
        token,
        "{repository(owner:\"graphql-alice\",name:\"proj\"){discussion(number:999999){id}}}",
    )
    .await;
    assert_eq!(
        body["errors"][0]["message"],
        "Could not resolve to a Discussion with the number of 999999."
    );
    // GitHub's default ownerAffiliations is [OWNER, COLLABORATOR]; a missing team is null.
    let data = query(
        router,
        token,
        "{organization(login:\"graphql-org\"){repositories(first:1){totalCount} team(slug:\"missing\"){id}}}",
        json!({}),
    )
    .await;
    assert!(data["organization"]["repositories"]["totalCount"].is_number());
    assert_eq!(data["organization"]["team"], Value::Null);
    let (status, saved) = request(
        router,
        Some(token),
        "PUT",
        "/api/v3/repos/graphql-alice/proj/contents/docs/guide.md",
        json!({"message":"Add a guide","content":"aGVsbG8K"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let data = query(
        router,
        token,
        "{repository(owner:\"graphql-alice\",name:\"proj\"){tree:object(expression:\"HEAD:docs\"){... on Tree{entries{name path}}} absent:object(expression:\"HEAD:docs/missing\"){id} gone:object(expression:\"HEAD:nowhere/missing\"){id}}}",
        json!({}),
    )
    .await;
    assert_eq!(
        data["repository"]["tree"]["entries"],
        json!([{"name":"guide.md","path":"docs/guide.md"}])
    );
    assert_eq!(data["repository"]["absent"], Value::Null);
    assert_eq!(data["repository"]["gone"], Value::Null);
}
