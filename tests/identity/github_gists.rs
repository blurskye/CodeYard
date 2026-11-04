//! GitHub gist contracts against real Git storage and each database engine.
use super::github::request;
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};

use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use tower::ServiceExt;

// Fixtures share the process-wide upload and mutation budgets; each scenario tests its own
// installation, not overload caused by unrelated installations running in this test binary.
static GIST_FIXTURES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
pub(super) async fn fixture_slot() -> tokio::sync::SemaphorePermit<'static> {
    GIST_FIXTURES.acquire().await.expect("gist fixture budget")
}

async fn contract(pg: bool) {
    let _fixture = fixture_slot().await;
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let a = Client::login(&router, f.pool(), alice).await;
    let b = Client::login(&router, f.pool(), bob).await;
    let mut credentials = Vec::new();
    for (actor, session, scope) in [
        (alice, &a.session, "write"),
        (bob, &b.session, "write"),
        (alice, &a.session, "read"),
    ] {
        let token = tokens::create(
            f.pool(),
            actor,
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
    let own = Some(credentials[0].as_str());
    let other = Some(credentials[1].as_str());
    let read = Some(credentials[2].as_str());
    request(
        &router,
        "gists/create",
        "POST",
        "/api/v3/gists",
        read,
        json!({"files":{"a.txt":{"content":"denied"}}}),
        403,
    )
    .await;
    for files in [
        json!({}),
        json!({"../escape":{"content":"no"}}),
        json!({".git":{"content":"no"}}),
        json!({"a.txt":{"content":1}}),
    ] {
        request(
            &router,
            "gists/create",
            "POST",
            "/api/v3/gists",
            own,
            json!({"files":files}),
            422,
        )
        .await;
    }
    let (headers,created)=request(&router,"gists/create","POST","/api/v3/gists",own,json!({"description":"Secret notes","files":{"hello.txt":{"content":"first\n"},"script.rs":{"content":"fn main() {}\n"}}}),201).await;
    let id = created["id"].as_str().unwrap();
    assert_eq!(id.len(), 32);
    let path = format!("/api/v3/gists/{id}");
    assert!(headers["location"].to_str().unwrap().ends_with(&path));
    assert_eq!(created["public"], false);
    assert_eq!(created["owner"]["id"], alice);
    assert_eq!(created["files"]["hello.txt"]["content"], "first\n");
    assert_eq!(created["files"]["script.rs"]["language"], "Rust");
    let first = created["history"][0]["version"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(first.len(), 40);
    assert_eq!(created["history"][0]["user"]["id"], alice);
    let record = codeyard_db_repos::gists::get(f.pool(), id)
        .await
        .unwrap()
        .unwrap();
    let backing = f.repository(record.repo_id).await;
    assert!(backing.is_gist);
    assert!(!backing.is_private);
    assert!(std::path::Path::new(&backing.disk_path).exists());
    assert!(codeyard_db_repos::repositories::records::get_repository(
        f.pool(),
        "alice",
        &backing.name
    )
    .await
    .unwrap()
    .is_none());
    assert_eq!(
        codeyard_db_repos::repositories::records::count_user_repositories(
            f.pool(),
            alice,
            Some(&a.session)
        )
        .await
        .unwrap(),
        0
    );
    let (_, anonymous) = request(&router, "gists/get", "GET", &path, None, Value::Null, 200).await;
    assert_eq!(anonymous["files"], created["files"]);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&path)
                .header("if-none-match", super_etag(&router, &path).await)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 304);
    assert!(to_bytes(response.into_body(), 1024)
        .await
        .unwrap()
        .is_empty());
    let (_,public)=request(&router,"gists/create","POST","/api/v3/gists",own,json!({"public":"true","description":"A public gist","files":{"public.md":{"content":"# Hello\n"}}}),201).await;
    let public_id = public["id"].as_str().unwrap();
    for operation in ["gists/list", "gists/list-public"] {
        let url = if operation == "gists/list" {
            "/api/v3/gists"
        } else {
            "/api/v3/gists/public"
        };
        let (_, list) = request(&router, operation, "GET", url, None, Value::Null, 200).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["id"], public_id);
        assert!(list[0]["files"]["public.md"].get("content").is_none());
    }
    let (headers, list) = request(
        &router,
        "gists/list",
        "GET",
        "/api/v3/gists?per_page=1",
        own,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (_, list) = request(
        &router,
        "gists/list-for-user",
        "GET",
        "/api/v3/users/alice/gists",
        own,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], public_id);
    let (_, list) = request(
        &router,
        "gists/list-public",
        "GET",
        "/api/v3/gists/public?since=2099-01-01T00%3A00%3A00Z",
        None,
        Value::Null,
        200,
    )
    .await;
    assert!(list.as_array().unwrap().is_empty());
    request(
        &router,
        "gists/list-public",
        "GET",
        "/api/v3/gists/public?since=not-a-date",
        None,
        Value::Null,
        422,
    )
    .await;
    request(
        &router,
        "gists/check-is-starred",
        "GET",
        &format!("{path}/star"),
        other,
        Value::Null,
        404,
    )
    .await;
    for _ in 0..2 {
        request(
            &router,
            "gists/star",
            "PUT",
            &format!("{path}/star"),
            other,
            Value::Null,
            204,
        )
        .await;
    }
    request(
        &router,
        "gists/check-is-starred",
        "GET",
        &format!("{path}/star"),
        other,
        Value::Null,
        204,
    )
    .await;
    let (_, stars) = request(
        &router,
        "gists/list-starred",
        "GET",
        "/api/v3/gists/starred",
        other,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(stars[0]["id"], id);
    request(
        &router,
        "gists/update",
        "PATCH",
        &path,
        other,
        json!({"description":"Not mine"}),
        404,
    )
    .await;
    request(
        &router,
        "gists/update",
        "PATCH",
        &path,
        own,
        json!({"files":{"hello.txt":{"filename":"script.rs"}}}),
        422,
    )
    .await;
    let (_,updated)=request(&router,"gists/update","PATCH",&path,own,json!({"description":"Revised notes","files":{"hello.txt":{"filename":"renamed.txt","content":"second\n"},"script.rs":null,"added.txt":{"content":"added\n"}}}),200).await;
    assert_eq!(updated["description"], "Revised notes");
    assert!(updated["files"].get("hello.txt").is_none());
    assert!(updated["files"].get("script.rs").is_none());
    assert_eq!(updated["files"]["renamed.txt"]["content"], "second\n");
    assert_ne!(updated["history"][0]["version"], first);
    let (_, old) = request(
        &router,
        "gists/get-revision",
        "GET",
        &format!("{path}/{}", &first[..12]),
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(old["files"]["hello.txt"]["content"], "first\n");
    assert_eq!(old["history"][0]["version"], first);
    assert!(old["files"].get("added.txt").is_none());
    request(
        &router,
        "gists/get-revision",
        "GET",
        &format!("{path}/abcdef0"),
        None,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "gists/get-revision",
        "GET",
        &format!("{path}/not-a-sha"),
        None,
        Value::Null,
        422,
    )
    .await;
    let (_, commits) = request(
        &router,
        "gists/list-commits",
        "GET",
        &format!("{path}/commits"),
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(commits.as_array().unwrap().len(), 2);
    assert_eq!(commits[1]["version"], first);
    assert!(commits[0]["change_status"]["additions"].as_i64().unwrap() > 0);
    request(
        &router,
        "gists/update",
        "PATCH",
        &path,
        own,
        json!({"files":{"renamed.txt":null,"added.txt":null}}),
        422,
    )
    .await;
    let (_, comment) = request(
        &router,
        "gists/create-comment",
        "POST",
        &format!("{path}/comments"),
        other,
        json!({"body":"**Useful** <script>alert(1)</script>"}),
        201,
    )
    .await;
    let comment_path = format!("{path}/comments/{}", comment["id"].as_i64().unwrap());
    assert_eq!(comment["user"]["id"], bob);
    assert_eq!(comment["author_association"], "NONE");
    request(
        &router,
        "gists/get-comment",
        "GET",
        &comment_path,
        None,
        Value::Null,
        200,
    )
    .await;
    request(
        &router,
        "gists/update-comment",
        "PATCH",
        &comment_path,
        own,
        json!({"body":"Not my comment"}),
        404,
    )
    .await;
    let (_, comment) = request(
        &router,
        "gists/update-comment",
        "PATCH",
        &comment_path,
        other,
        json!({"body":"Edited"}),
        200,
    )
    .await;
    assert_eq!(comment["body"], "Edited");
    let (_, comments) = request(
        &router,
        "gists/list-comments",
        "GET",
        &format!("{path}/comments?per_page=1"),
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(comments.as_array().unwrap().len(), 1);
    let (_, fork) = request(
        &router,
        "gists/fork",
        "POST",
        &format!("{path}/forks"),
        other,
        Value::Null,
        201,
    )
    .await;
    let fork_id = fork["id"].as_str().unwrap();
    assert_ne!(fork_id, id);
    assert_eq!(fork["public"], false);
    assert_eq!(
        fork["files"]["renamed.txt"]["content"],
        updated["files"]["renamed.txt"]["content"]
    );
    assert_eq!(fork["fork_of"]["id"], id);
    request(
        &router,
        "gists/fork",
        "POST",
        &format!("{path}/forks"),
        other,
        Value::Null,
        422,
    )
    .await;
    request(
        &router,
        "gists/fork",
        "POST",
        &format!("{path}/forks"),
        own,
        Value::Null,
        422,
    )
    .await;
    let (_, forks) = request(
        &router,
        "gists/list-forks",
        "GET",
        &format!("{path}/forks"),
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(forks[0]["id"], fork_id);
    let (_, with_fork) = request(&router, "gists/get", "GET", &path, None, Value::Null, 200).await;
    assert_eq!(with_fork["forks"][0]["user"]["id"], bob);
    request(
        &router,
        "gists/unstar",
        "DELETE",
        &format!("{path}/star"),
        other,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "gists/check-is-starred",
        "GET",
        &format!("{path}/star"),
        other,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "gists/delete-comment",
        "DELETE",
        &comment_path,
        own,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "gists/get-comment",
        "GET",
        &comment_path,
        None,
        Value::Null,
        404,
    )
    .await;
    let (_, profile) = request(
        &router,
        "users/get-authenticated",
        "GET",
        "/api/v3/user",
        own,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(profile["public_gists"], 1);
    assert_eq!(profile["private_gists"], 1);
    assert_eq!(profile["public_repos"], 0);
    request(
        &router,
        "gists/delete",
        "DELETE",
        &path,
        own,
        Value::Null,
        204,
    )
    .await;
    assert!(!std::path::Path::new(&backing.disk_path).exists());
    assert!(codeyard_db_repos::gists::get(f.pool(), id)
        .await
        .unwrap()
        .is_none());
    request(&router, "gists/get", "GET", &path, None, Value::Null, 404).await;
    let (_, surviving) = request(
        &router,
        "gists/get",
        "GET",
        &format!("/api/v3/gists/{fork_id}"),
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(surviving["files"]["renamed.txt"]["content"], "second\n");
    assert!(surviving["fork_of"].is_null());
    f.finish().await;
}
async fn super_etag(router: &axum::Router, path: &str) -> String {
    let (headers, _) = request(router, "gists/get", "GET", path, None, Value::Null, 200).await;
    headers["etag"].to_str().unwrap().to_owned()
}
#[tokio::test]
async fn sqlite_gists_contract() {
    contract(false).await
}
#[tokio::test]
async fn postgres_gists_contract() {
    contract(true).await
}
