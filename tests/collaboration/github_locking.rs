use super::github::{document, request};
use crate::support::{database::Database, http_client::Client};
use axum::{body::to_bytes, http::StatusCode};
use codeyard::backend::{
    db,
    identity::tokens::{self, TokenInput},
};
use serde_json::{json, Value};

async fn locking(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), alice).await;
    let other_browser = Client::login(&router, f.pool(), bob).await;
    let mut credentials = Vec::new();
    for (id, scope, session) in [
        (alice, "write", browser.session.as_str()),
        (alice, "read", browser.session.as_str()),
        (bob, "write", other_browser.session.as_str()),
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
    let issue = db::issues::create_issue(f.pool(), repo.id, alice, "Issue", "")
        .await
        .unwrap();
    let base_commit = browser.save("main", "base.txt", "unborn").await;
    browser.branch("topic", &base_commit).await;
    browser.save("topic", "feature.txt", &base_commit).await;
    let pull = browser.open_pull("Pull", "topic", "main").await;
    let pull_number = pull["number"].as_i64().unwrap() as i32;
    let base = "/api/v3/repos/alice/proj";
    for (number, family) in [(issue.number, "issues"), (pull_number, "pulls")] {
        let path = format!("{base}/issues/{number}");
        let lock = format!("{path}/lock");
        let created = document(
            request(
                &router,
                "POST",
                &format!("{path}/comments"),
                Some(write),
                json!({"body":"Before locking"}),
            )
            .await,
            "issues/create-comment",
            201,
        )
        .await;
        let comment = format!("{base}/issues/comments/{}", created["id"]);
        document(
            request(
                &router,
                "PUT",
                &lock,
                Some(write),
                json!({"lock_reason":"invalid"}),
            )
            .await,
            "issues/lock",
            422,
        )
        .await;
        for token in [read, other] {
            document(
                request(&router, "PUT", &lock, Some(token), json!({})).await,
                "issues/lock",
                403,
            )
            .await;
        }
        for _ in 0..2 {
            let response = request(
                &router,
                "PUT",
                &lock,
                Some(write),
                json!({"lock_reason":"spam"}),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            assert!(to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .is_empty());
        }
        let got = document(
            request(&router, "GET", &path, Some(read), Value::Null).await,
            "issues/get",
            200,
        )
        .await;
        assert_eq!(got["locked"], true);
        assert_eq!(got["active_lock_reason"], "spam");
        if family == "pulls" {
            let got = document(
                request(
                    &router,
                    "GET",
                    &format!("{base}/pulls/{number}"),
                    Some(read),
                    Value::Null,
                )
                .await,
                "pulls/get",
                200,
            )
            .await;
            assert_eq!(got["locked"], true);
        }
        let events = document(
            request(
                &router,
                "GET",
                &format!("{path}/events"),
                Some(read),
                Value::Null,
            )
            .await,
            "issues/list-events",
            200,
        )
        .await;
        assert_eq!(
            events
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| event["event"] == "locked")
                .count(),
            1
        );
        for token in [write, other] {
            assert_eq!(
                request(
                    &router,
                    "POST",
                    &format!("{path}/comments"),
                    Some(token),
                    json!({"body":"Not allowed"})
                )
                .await
                .status(),
                StatusCode::FORBIDDEN
            );
        }
        for method in ["PATCH", "DELETE"] {
            assert_eq!(
                request(
                    &router,
                    method,
                    &comment,
                    Some(write),
                    json!({"body":"Not allowed"})
                )
                .await
                .status(),
                StatusCode::FORBIDDEN
            );
        }
        let page = format!("/alice/proj/{family}/{number}");
        let html = browser.page_ok(&page).await;
        assert!(html.contains("This conversation is locked."));
        assert!(html.contains("Unlock conversation"));
        assert!(html.contains(&format!("action=\"{page}/lock\"")));
        assert!(html.contains("name=\"csrf_token\""));
        let denied = browser
            .exchange(
                "POST",
                &format!("{page}/lock"),
                "application/x-www-form-urlencoded",
                b"locked=false".to_vec(),
            )
            .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN);
        let reply = browser
            .form_reply(&format!("{page}/lock"), &[("locked", "false")])
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
        let got = document(
            request(&router, "GET", &path, Some(read), Value::Null).await,
            "issues/get",
            200,
        )
        .await;
        assert_eq!(got["locked"], false);
        assert_eq!(got["active_lock_reason"], Value::Null);
        document(
            request(
                &router,
                "POST",
                &format!("{path}/comments"),
                Some(write),
                json!({"body":"After unlocking"}),
            )
            .await,
            "issues/create-comment",
            201,
        )
        .await;
        let reply = browser
            .form_reply(
                &format!("{page}/lock"),
                &[("locked", "true"), ("lock_reason", "resolved")],
            )
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
        let (_, model) = browser.model(&page).await;
        let model = serde_json::to_string(&model).unwrap();
        assert!(model.contains("\"locked\":true"));
        assert!(model.contains("\"lock_reason\":\"resolved\""));
        let response = request(&router, "DELETE", &lock, Some(write), Value::Null).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
    }
    f.execute(
        "UPDATE repositories SET is_private=true WHERE id=$1",
        &[repo.id],
    )
    .await;
    document(
        request(
            &router,
            "PUT",
            &format!("{base}/issues/{}/lock", issue.number),
            Some(other),
            json!({}),
        )
        .await,
        "issues/lock",
        404,
    )
    .await;
    f.finish().await;
}
#[tokio::test]
async fn github_locking_sqlite() {
    locking(false).await;
}
#[tokio::test]
async fn github_locking_postgres() {
    locking(true).await;
}
