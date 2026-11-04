use super::github::{document, request};
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::{body::to_bytes, http::StatusCode};

use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};

async fn pins(pg: bool) {
    crate::support::fake_webui_bundle::install();
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
    let issue = codeyard_db_repos::issues::create_issue(f.pool(), repo.id, alice, "Issue", "")
        .await
        .unwrap();
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), alice).await;
    let outsider = Client::login(&router, f.pool(), bob).await;
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
    let base_commit = browser.save("main", "base.txt", "unborn").await;
    browser.branch("topic", &base_commit).await;
    browser.save("topic", "feature.txt", &base_commit).await;
    let pull = browser.open_pull("Pull", "topic", "main").await;
    let pull_number = pull["number"].as_i64().unwrap() as i32;
    let base = "/api/v3/repos/alice/proj";
    let mut first_comment = 0;
    for (number, family) in [(issue.number, "issues"), (pull_number, "pulls")] {
        let item = format!("{base}/issues/{number}");
        let page = format!("/alice/proj/{family}/{number}");
        let first = document(
            request(
                &router,
                "POST",
                &format!("{item}/comments"),
                Some(write),
                json!({"body":"Pinned words <script>bad()</script>"}),
            )
            .await,
            "issues/create-comment",
            201,
        )
        .await;
        let second = document(
            request(
                &router,
                "POST",
                &format!("{item}/comments"),
                Some(write),
                json!({"body":"Other words"}),
            )
            .await,
            "issues/create-comment",
            201,
        )
        .await;
        let id = first["id"].as_i64().unwrap();
        first_comment = id;
        let comment = format!("{base}/issues/comments/{id}");
        let pin = format!("{comment}/pin");
        document(
            request(&router, "PUT", &pin, Some(read), Value::Null).await,
            "issues/pin-comment",
            403,
        )
        .await;
        document(
            request(&router, "PUT", &pin, Some(other), Value::Null).await,
            "issues/pin-comment",
            404,
        )
        .await;
        let pinned = document(
            request(&router, "PUT", &pin, Some(write), Value::Null).await,
            "issues/pin-comment",
            200,
        )
        .await;
        assert_eq!(pinned["pin"]["pinned_by"]["login"], "alice");
        assert!(pinned["pin"]["pinned_at"].is_string());
        let duplicate = document(
            request(&router, "PUT", &pin, Some(write), Value::Null).await,
            "issues/pin-comment",
            200,
        )
        .await;
        assert_eq!(pinned["pin"], duplicate["pin"]);
        let second_pin = format!("{base}/issues/comments/{}/pin", second["id"]);
        document(
            request(&router, "PUT", &second_pin, Some(write), Value::Null).await,
            "issues/pin-comment",
            422,
        )
        .await;
        let got = document(
            request(&router, "GET", &item, Some(read), Value::Null).await,
            "issues/get",
            200,
        )
        .await;
        assert_eq!(got["pinned_comment"]["id"], id);
        let html = browser.page_ok(&page).await;
        assert!(html.contains("aria-label=\"Pinned comment\""));
        assert!(html.contains("Unpin comment"));
        assert!(!html.contains("<script>bad()</script>"));
        let (_, model) = browser.model(&page).await;
        let serialized = serde_json::to_string(&model).unwrap();
        assert!(serialized.contains("\"pinned\":{\"id\":"));
        document(
            request(&router, "DELETE", &pin, Some(read), Value::Null).await,
            "issues/unpin-comment",
            403,
        )
        .await;
        let response = request(&router, "DELETE", &pin, Some(write), Value::Null).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
        let got = document(
            request(&router, "GET", &comment, Some(read), Value::Null).await,
            "issues/get-comment",
            200,
        )
        .await;
        assert_eq!(got["pin"], Value::Null);
        let native = format!("{page}/comments/{}/pin", second["id"].as_i64().unwrap() / 2);
        let denied = browser
            .exchange(
                "POST",
                &native,
                "application/x-www-form-urlencoded",
                b"pinned=true".to_vec(),
            )
            .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN);
        let reply = browser.form_reply(&native, &[("pinned", "true")]).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
        let got = document(
            request(&router, "GET", &item, Some(read), Value::Null).await,
            "issues/get",
            200,
        )
        .await;
        assert_eq!(got["pinned_comment"]["id"], second["id"]);
        let reply = browser.form_reply(&native, &[("pinned", "false")]).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
        document(
            request(&router, "PUT", &second_pin, Some(write), Value::Null).await,
            "issues/pin-comment",
            200,
        )
        .await;
        let response = request(
            &router,
            "DELETE",
            &format!("{base}/issues/comments/{}", second["id"]),
            Some(write),
            Value::Null,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let got = document(
            request(&router, "GET", &item, Some(read), Value::Null).await,
            "issues/get",
            200,
        )
        .await;
        assert_eq!(got["pinned_comment"], Value::Null);
    }
    f.execute(
        "UPDATE repositories SET is_archived=true WHERE id=$1",
        &[repo.id],
    )
    .await;
    document(
        request(
            &router,
            "PUT",
            &format!("{base}/issues/comments/{first_comment}/pin"),
            Some(write),
            Value::Null,
        )
        .await,
        "issues/pin-comment",
        403,
    )
    .await;
    f.finish().await;
}
#[tokio::test]
async fn github_pins_sqlite() {
    pins(false).await;
}
#[tokio::test]
async fn github_pins_postgres() {
    pins(true).await;
}
