use super::github::{document, request};
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::http::StatusCode;
use base64::{engine::general_purpose::STANDARD, Engine};

use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
fn url(kind: &str, q: &str, extra: &[(&str, &str)]) -> String {
    let mut pairs = url::form_urlencoded::Serializer::new(String::new());
    pairs.append_pair("q", q);
    for (k, v) in extra {
        pairs.append_pair(k, v);
    }
    format!("/api/v3/search/{kind}?{}", pairs.finish())
}
async fn save(
    client: &Client,
    repo: &str,
    path: &str,
    text: &str,
    tip: &str,
    message: &str,
) -> String {
    let value=client.expect("POST",&format!("/api/repos/alice/{repo}/contents/{path}"),json!({"content":STANDARD.encode(text),"message":message,"branch":"main","expected_revision":tip}),StatusCode::CREATED).await;
    value["sha"].as_str().unwrap().into()
}
async fn search(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    for (name, private) in [
        ("proj", false),
        ("other", false),
        ("secret", true),
        ("empty", false),
    ] {
        codeyard_repos_core::repositories::lifecycle::provision(
            (f.pool(), None),
            root.path(),
            codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
            name,
            None,
            private,
            None,
        )
        .await
        .unwrap();
    }
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), alice).await;
    let outsider = Client::login(&router, f.pool(), bob).await;
    let main = save(
        &browser,
        "proj",
        "src/main.rs",
        "// NEEDLE道路\n// another needle\n// separate token\n",
        "unborn",
        "Needle 道路 main",
    )
    .await;
    let lib = save(
        &browser,
        "proj",
        "src/lib.rs",
        "// needle library\n",
        &main,
        "Needle library",
    )
    .await;
    let tip = save(
        &browser,
        "proj",
        "notes.txt",
        "needle in notes\n",
        &lib,
        "Needle notes",
    )
    .await;
    save(
        &browser,
        "other",
        "README.md",
        "needle outside\n",
        "unborn",
        "Needle outside",
    )
    .await;
    save(
        &browser,
        "secret",
        "main.rs",
        "confidential needle\n",
        "unborn",
        "Confidential needle",
    )
    .await;
    let mut credentials = Vec::new();
    for (id, scope, session) in [
        (alice, "write", browser.session.as_str()),
        (alice, "read", browser.session.as_str()),
        (bob, "read", outsider.session.as_str()),
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
    assert_eq!(
        request(
            &router,
            "GET",
            &url("code", "needle", &[]),
            None,
            Value::Null
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let code = document(
        request(
            &router,
            "GET",
            &url("code", "needle", &[]),
            Some(other),
            Value::Null,
        )
        .await,
        "search/code",
        200,
    )
    .await;
    assert_eq!(code["total_count"], 4, "{code}");
    assert_eq!(code["incomplete_results"], false);
    assert_eq!(code["indexing"], "live");
    assert!(code["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|v| v["repository"]["private"] == false));
    let owner = document(
        request(
            &router,
            "GET",
            &url("code", "needle", &[]),
            Some(read),
            Value::Null,
        )
        .await,
        "search/code",
        200,
    )
    .await;
    assert_eq!(owner["total_count"], 5);
    for (q, total) in [
        ("repo:alice/proj needle separate", 1),
        ("repo:alice/proj needle language:rust", 2),
        ("repo:alice/proj needle extension:txt", 1),
        ("repo:alice/proj src in:path", 2),
        ("repo:alice/proj filename:main.rs", 1),
        ("repo:alice/proj 道路", 1),
        ("repo:alice/proj needle -separate", 2),
        (
            "repo:alice/proj (filename:main.rs OR filename:lib.rs) needle",
            2,
        ),
        ("repo:alice/empty needle", 0),
        ("repo:alice/secret needle", 0),
        ("repo:alice/proj needle size:>100000", 0),
    ] {
        let value = document(
            request(
                &router,
                "GET",
                &url("code", q, &[]),
                Some(other),
                Value::Null,
            )
            .await,
            "search/code",
            200,
        )
        .await;
        assert_eq!(value["total_count"], total, "{q}: {value}");
        assert_eq!(value["incomplete_results"], false, "{q}: {value}");
    }
    let cross_lines = document(
        request(
            &router,
            "GET",
            &url("code", "repo:alice/proj needle separate", &[]),
            Some(read),
            Value::Null,
        )
        .await,
        "search/code",
        200,
    )
    .await;
    assert!(cross_lines["items"][0]["line_numbers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|line| line == "3"));
    let sha = cross_lines["items"][0]["sha"].as_str().unwrap();
    let blob = document(
        request(
            &router,
            "GET",
            &format!("/api/v3/repos/alice/proj/git/blobs/{sha}"),
            Some(read),
            Value::Null,
        )
        .await,
        "git/get-blob",
        200,
    )
    .await;
    assert_eq!(blob["sha"], sha);
    assert!(String::from_utf8(
        STANDARD
            .decode(blob["content"].as_str().unwrap().replace('\n', ""))
            .unwrap()
    )
    .unwrap()
    .contains("NEEDLE道路"));
    let one = request(
        &router,
        "GET",
        &url("code", "needle", &[("per_page", "1")]),
        Some(other),
        Value::Null,
    )
    .await;
    assert!(one.headers()["link"].to_str().unwrap().contains("page=2"));
    let one = document(one, "search/code", 200).await;
    let two = document(
        request(
            &router,
            "GET",
            &url("code", "needle", &[("per_page", "1"), ("page", "2")]),
            Some(other),
            Value::Null,
        )
        .await,
        "search/code",
        200,
    )
    .await;
    assert_ne!(
        (
            one["items"][0]["repository"]["id"].clone(),
            one["items"][0]["path"].clone()
        ),
        (
            two["items"][0]["repository"]["id"].clone(),
            two["items"][0]["path"].clone()
        )
    );
    document(
        request(
            &router,
            "POST",
            &format!("/api/v3/repos/alice/proj/commits/{tip}/comments"),
            Some(write),
            json!({"body":"Search includes live conversation counts"}),
        )
        .await,
        "repos/create-commit-comment",
        201,
    )
    .await;
    let commits = document(
        request(
            &router,
            "GET",
            &url("commits", "needle", &[]),
            None,
            Value::Null,
        )
        .await,
        "search/commits",
        200,
    )
    .await;
    assert_eq!(commits["total_count"], 4);
    assert_eq!(commits["incomplete_results"], false);
    let owned = document(
        request(
            &router,
            "GET",
            &url("commits", "needle", &[]),
            Some(read),
            Value::Null,
        )
        .await,
        "search/commits",
        200,
    )
    .await;
    assert_eq!(owned["total_count"], 5);
    for (q, total) in [
        ("repo:alice/proj needle", 3),
        ("repo:alice/proj needle author:alice", 3),
        ("repo:alice/proj needle committer:alice", 3),
        ("repo:alice/proj needle author-name:alice", 3),
        ("repo:alice/proj needle merge:false", 3),
        ("repo:alice/proj needle merge:true", 0),
        ("repo:alice/proj needle author-date:>=2000-01-01", 3),
        (
            "repo:alice/proj needle committer-date:2000-01-01..2099-12-31",
            3,
        ),
        ("repo:alice/proj 道路", 1),
        ("repo:alice/secret needle", 0),
        ("repo:alice/empty needle", 0),
    ] {
        // Qualifier cases are independent conversations, not a rate-limit stress test.
        f.execute("DELETE FROM github_api_limits WHERE resource='search'", &[])
            .await;
        let value = document(
            request(&router, "GET", &url("commits", q, &[]), None, Value::Null).await,
            "search/commits",
            200,
        )
        .await;
        assert_eq!(value["total_count"], total, "{q}: {value}");
    }
    let exact = document(
        request(
            &router,
            "GET",
            &url(
                "commits",
                &format!("repo:alice/proj hash:{}", &tip[..8]),
                &[],
            ),
            Some(read),
            Value::Null,
        )
        .await,
        "search/commits",
        200,
    )
    .await;
    assert_eq!(exact["total_count"], 1);
    assert_eq!(exact["items"][0]["sha"], tip);
    assert_eq!(exact["items"][0]["commit"]["comment_count"], 1);
    assert_eq!(exact["items"][0]["author"]["login"], "alice");
    let mine = document(
        request(
            &router,
            "GET",
            &url("commits", "needle author:@me", &[]),
            Some(read),
            Value::Null,
        )
        .await,
        "search/commits",
        200,
    )
    .await;
    assert_eq!(mine["total_count"], 5);
    assert_eq!(
        request(
            &router,
            "GET",
            &url("commits", "author:@me", &[]),
            None,
            Value::Null
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    for (kind, op) in [("code", "search/code"), ("commits", "search/commits")] {
        for q in ["", "\"unterminated", "unknown:term"] {
            document(
                request(&router, "GET", &url(kind, q, &[]), Some(read), Value::Null).await,
                op,
                422,
            )
            .await;
        }
        document(
            request(
                &router,
                "GET",
                &url(kind, "needle", &[("page", "1001")]),
                Some(read),
                Value::Null,
            )
            .await,
            op,
            422,
        )
        .await;
    }
    document(
        request(
            &router,
            "GET",
            &url("code", "\"needle\nmissing\"", &[]),
            Some(read),
            Value::Null,
        )
        .await,
        "search/code",
        422,
    )
    .await;
    let native = browser.page_ok("/search?type=code&q=needle").await;
    assert!(native.contains("needle"));
    f.finish().await;
}
#[tokio::test]
async fn github_content_search_sqlite() {
    search(false).await;
}
#[tokio::test]
async fn github_content_search_postgres() {
    search(true).await;
}
