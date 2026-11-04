use super::github::exchange;
use crate::support::database::Database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client::visible_text;
use crate::support::http_client::Client;
use axum::{http::StatusCode, Router};
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_app::repositories::lifecycle;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};

async fn call(
    router: &Router,
    operation: &str,
    method: &str,
    url: &str,
    token: &str,
    body: Value,
    status: StatusCode,
) -> Value {
    let (code, _, body) = exchange(router, operation, method, url, Some(token), body).await;
    assert_eq!(code, status, "{operation}: {body}");
    body
}
async fn contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
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
                name: "ranges",
                scope: "write",
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let original = (1..=30).map(|n| format!("line {n}\n")).collect::<String>();
    let base = alice
        .save_text("main", "range.txt", &original, "unborn")
        .await;
    alice.branch("feature", &base).await;
    let changed = original
        .replace("line 2\n", "changed 2\n")
        .replace("line 25\n", "changed 25\n");
    let head = alice
        .update_text("feature", "range.txt", &changed, &base)
        .await;
    let pull = call(
        &router,
        "pulls/create",
        "POST",
        "/api/v3/repos/alice/proj/pulls",
        &credentials[0],
        json!({"title":"Ranges","head":"feature","base":"main","maintainer_can_modify":false}),
        StatusCode::CREATED,
    )
    .await;
    let number = pull["number"].as_i64().unwrap();
    let url = format!("/api/v3/repos/alice/proj/pulls/{number}");
    let valid = json!({"body":"A real multiline comment","commit_id":head,"path":"range.txt","start_line":2,"start_side":"RIGHT","line":4,"side":"RIGHT"});
    let created = call(
        &router,
        "pulls/create-review-comment",
        "POST",
        &format!("{url}/comments"),
        &credentials[1],
        valid.clone(),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(created["start_line"], 2);
    assert_eq!(created["start_side"], "RIGHT");
    assert_eq!(created["original_start_line"], 2);
    let id = created["id"].as_i64().unwrap();
    let stored =
        codeyard_db_repos::pull_requests::reviews::get_pull_request_comment_by_id(f.pool(), id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(stored.start_line, Some(2));
    assert_eq!(stored.start_side.as_deref(), Some("RIGHT"));
    let reply = call(
        &router,
        "pulls/create-reply-for-review-comment",
        "POST",
        &format!("{url}/comments/{id}/replies"),
        &credentials[0],
        json!({"body":"Reply inherits the entire range"}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(reply["start_line"], 2);
    assert_eq!(reply["line"], 4);
    assert_eq!(reply["start_side"], "RIGHT");
    let edited = call(
        &router,
        "pulls/update-review-comment",
        "PATCH",
        &format!("/api/v3/repos/alice/proj/pulls/comments/{id}"),
        &credentials[1],
        json!({"body":"Range unchanged by an edit"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(edited["start_line"], 2);
    for edit in [
        json!({"start_line":4,"line":2}),
        json!({"start_line":2,"line":25}),
        json!({"start_line":0}),
        json!({"start_side":"INVALID"}),
        json!({"start_side":null}),
        json!({"subject_type":"file"}),
    ] {
        let mut invalid = valid.clone();
        for (k, v) in edit.as_object().unwrap() {
            invalid[k] = v.clone();
        }
        call(
            &router,
            "pulls/create-review-comment",
            "POST",
            &format!("{url}/comments"),
            &credentials[1],
            invalid,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let mut cross = valid.clone();
    cross["start_side"] = json!("LEFT");
    cross["line"] = json!(2);
    let cross = call(
        &router,
        "pulls/create-review-comment",
        "POST",
        &format!("{url}/comments"),
        &credentials[1],
        cross,
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(cross["start_side"], "LEFT");
    assert_eq!(cross["side"], "RIGHT");
    let pending=call(&router,"pulls/create-review","POST",&format!("{url}/reviews"),&credentials[1],json!({"commit_id":head,"comments":[{"body":"Private range","path":"range.txt","start_line":2,"start_side":"RIGHT","line":4,"side":"RIGHT"}]}),StatusCode::OK).await;
    let review = pending["id"].as_i64().unwrap();
    let comments = call(
        &router,
        "pulls/list-comments-for-review",
        "GET",
        &format!("{url}/reviews/{review}/comments"),
        &credentials[1],
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(comments[0]["start_line"], 2);
    call(
        &router,
        "pulls/list-comments-for-review",
        "GET",
        &format!("{url}/reviews/{review}/comments"),
        &credentials[0],
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    call(
        &router,
        "pulls/submit-review",
        "POST",
        &format!("{url}/reviews/{review}/events"),
        &credentials[1],
        json!({"event":"COMMENT"}),
        StatusCode::OK,
    )
    .await;
    for view in ["", "?view=split"] {
        let page = bob
            .exchange(
                "GET",
                &format!("/alice/proj/pulls/{number}/diff{view}"),
                "text/html",
                Vec::new(),
            )
            .await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.text());
        let text = visible_text(&page.text());
        assert!(text.contains("Lines RIGHT 2–RIGHT 4"), "{text}");
    }
    let page = bob
        .exchange(
            "GET",
            &format!("/alice/proj/pulls/{number}/diff?path=range.txt&side=RIGHT&line=4"),
            "text/html",
            Vec::new(),
        )
        .await;
    assert!(page.text().contains("name=\"start_line\""));
    assert!(page.text().contains("name=\"start_side\""));
    for (start, status) in [("5", StatusCode::BAD_REQUEST), ("2", StatusCode::SEE_OTHER)] {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("csrf_token", bob.csrf.as_str()),
                ("repository_id", repo.id.to_string().as_str()),
                ("path", "range.txt"),
                ("side", "RIGHT"),
                ("line", "4"),
                ("start_line", start),
                ("start_side", "RIGHT"),
                ("commit_sha", head.as_str()),
                ("base_sha", base.as_str()),
                ("from", "diff"),
                ("intent", "comment"),
                ("body", "Native range form"),
            ])
            .finish();
        let result = bob
            .exchange(
                "POST",
                &format!("/alice/proj/pulls/{number}/comments?view=split"),
                "application/x-www-form-urlencoded",
                body.into_bytes(),
            )
            .await;
        assert_eq!(result.status, status, "{}", result.text());
        if status == StatusCode::BAD_REQUEST {
            assert!(result.text().contains("value=\"5\""));
            assert!(result.text().contains("Native range form"));
        }
    }
    f.finish().await;
}
#[tokio::test]
async fn sqlite_github_multiline() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_multiline() {
    contract(true).await;
}
