use super::github::{document, request};
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::{body::to_bytes, http::StatusCode};
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
async fn reactions(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_app::repositories::lifecycle::provision(
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
    let outsider = Client::login(&router, f.pool(), bob).await;
    browser.save("main", "README.md", "unborn").await;
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
    let base = "/api/v3/repos/alice/proj";
    let release = document(
        request(
            &router,
            "POST",
            &format!("{base}/releases"),
            Some(write),
            json!({"tag_name":"v1","name":"Release","body":"Notes"}),
        )
        .await,
        "repos/create-release",
        201,
    )
    .await;
    let id = release["id"].as_i64().unwrap();
    let path = format!("{base}/releases/{id}/reactions");
    let one = document(
        request(
            &router,
            "POST",
            &path,
            Some(write),
            json!({"content":"heart"}),
        )
        .await,
        "reactions/create-for-release",
        201,
    )
    .await;
    let duplicate = document(
        request(
            &router,
            "POST",
            &path,
            Some(write),
            json!({"content":"heart"}),
        )
        .await,
        "reactions/create-for-release",
        200,
    )
    .await;
    assert_eq!(one["id"], duplicate["id"]);
    let two = document(
        request(
            &router,
            "POST",
            &path,
            Some(other),
            json!({"content":"eyes"}),
        )
        .await,
        "reactions/create-for-release",
        201,
    )
    .await;
    for content in ["-1", "confused", "invalid"] {
        document(
            request(
                &router,
                "POST",
                &path,
                Some(write),
                json!({"content":content}),
            )
            .await,
            "reactions/create-for-release",
            422,
        )
        .await;
    }
    assert_eq!(
        request(
            &router,
            "POST",
            &path,
            Some(read),
            json!({"content":"heart"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let response = request(
        &router,
        "GET",
        &format!("{path}?per_page=1"),
        Some(read),
        Value::Null,
    )
    .await;
    assert!(response.headers()["link"]
        .to_str()
        .unwrap()
        .contains("page=2"));
    document(response, "reactions/list-for-release", 200).await;
    let filtered = document(
        request(
            &router,
            "GET",
            &format!("{path}?content=eyes"),
            None,
            Value::Null,
        )
        .await,
        "reactions/list-for-release",
        200,
    )
    .await;
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    assert_eq!(filtered[0]["user"]["login"], "bob");
    let release = document(
        request(
            &router,
            "GET",
            &format!("{base}/releases/{id}"),
            Some(read),
            Value::Null,
        )
        .await,
        "repos/get-release",
        200,
    )
    .await;
    assert_eq!(release["reactions"]["total_count"], 2);
    let html = browser.page_ok("/alice/proj/releases/tag/v1").await;
    assert!(html.contains(&format!("action=\"/alice/proj/releases/{id}/react\"")));
    assert!(!html.contains("value=\"-1\""));
    assert!(!html.contains("value=\"confused\""));
    let native = format!("/alice/proj/releases/{id}/react");
    let denied = browser
        .exchange(
            "POST",
            &native,
            "application/x-www-form-urlencoded",
            b"content=heart".to_vec(),
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let reply = browser.form_reply(&native, &[("content", "heart")]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let listed = document(
        request(&router, "GET", &path, Some(read), Value::Null).await,
        "reactions/list-for-release",
        200,
    )
    .await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let reply = browser.form_reply(&native, &[("content", "+1")]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let (_, model) = browser.model("/alice/proj/releases/tag/v1").await;
    assert!(serde_json::to_string(&model)
        .unwrap()
        .contains("\"mine\":true"));
    assert_eq!(
        request(
            &router,
            "DELETE",
            &format!("{path}/{}", two["id"]),
            Some(write),
            Value::Null
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let response = request(
        &router,
        "DELETE",
        &format!("{path}/{}", two["id"]),
        Some(other),
        Value::Null,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(to_bytes(response.into_body(), 1024)
        .await
        .unwrap()
        .is_empty());
    let draft = document(
        request(
            &router,
            "POST",
            &format!("{base}/releases"),
            Some(write),
            json!({"tag_name":"v2","draft":true}),
        )
        .await,
        "repos/create-release",
        201,
    )
    .await;
    let draft_id = draft["id"].as_i64().unwrap();
    let draft_path = format!("{base}/releases/{draft_id}/reactions");
    document(
        request(&router, "GET", &draft_path, Some(other), Value::Null).await,
        "reactions/list-for-release",
        404,
    )
    .await;
    assert!(db::reactions::create_reaction(
        f.pool(),
        bob,
        &db::users::accounts::AccountCredential::Session(&outsider.session),
        db::reactions::Target {
            repo_id: repo.id,
            item_id: draft_id,
            subject: db::reactions::Subject::Release,
            subject_id: draft_id
        },
        "heart"
    )
    .await
    .is_err());
    f.execute(
        "UPDATE repositories SET is_private=true WHERE id=$1",
        &[repo.id],
    )
    .await;
    document(
        request(&router, "GET", &path, Some(other), Value::Null).await,
        "reactions/list-for-release",
        404,
    )
    .await;
    f.execute(
        "UPDATE repositories SET is_archived=true WHERE id=$1",
        &[repo.id],
    )
    .await;
    assert_eq!(
        request(
            &router,
            "POST",
            &path,
            Some(write),
            json!({"content":"rocket"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    f.execute(
        "UPDATE repositories SET is_archived=false WHERE id=$1",
        &[repo.id],
    )
    .await;
    let response = request(
        &router,
        "DELETE",
        &format!("{base}/releases/{id}"),
        Some(write),
        Value::Null,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        db::reactions::reaction_counts(f.pool(), "release", &[id], alice)
            .await
            .unwrap()
            .is_empty()
    );
    f.finish().await;
}
#[tokio::test]
async fn github_release_reactions_sqlite() {
    reactions(false).await;
}
#[tokio::test]
async fn github_release_reactions_postgres() {
    reactions(true).await;
}
