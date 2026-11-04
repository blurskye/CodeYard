//! GitHub pull request reads through the real router on both engines.
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};

use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn request(
    router: &Router,
    operation: &str,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    exchange(router, operation, "GET", path, token, Value::Null).await
}

/// A plain-text response, for the diff and patch media types.
async fn text(router: &Router, path: &str, accept: &str) -> (StatusCode, String) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("accept", accept)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

pub(super) async fn exchange(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    input: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("accept", "application/vnd.github+json");
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
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("{status}: {}", String::from_utf8_lossy(&bytes)));
        assert_github_schema(operation, status.as_u16(), &body);
        body
    };
    (status, headers, body)
}

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    for (name, private) in [("proj", false), ("secret", true)] {
        codeyard_repos_core::repositories::lifecycle::provision(
            (f.pool(), None),
            root.path(),
            codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice_id),
            name,
            None,
            private,
            None,
        )
        .await
        .unwrap();
    }
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let reader = tokens::create(
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
    let outsider = tokens::create(
        f.pool(),
        bob_id,
        Some(&bob.session),
        TokenInput {
            name: "outsider",
            scope: "read",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let writer = tokens::create(
        f.pool(),
        alice_id,
        Some(&alice.session),
        TokenInput {
            name: "merger",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let other_writer = tokens::create(
        f.pool(),
        bob_id,
        Some(&bob.session),
        TokenInput {
            name: "not-a-merger",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let writer = format!("Bearer {}", writer.value);
    let other_writer = format!("Bearer {}", other_writer.value);
    let reader = format!("Bearer {}", reader.value);
    let outsider = format!("token {}", outsider.value);
    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature/a", &base).await;
    let first = alice.save("feature/a", "one.txt", &base).await;
    let head = alice.save("feature/a", "two.txt", &first).await;
    let pull = alice.open_pull("Feature A", "feature/a", "main").await;
    let number = pull["number"].as_i64().unwrap();
    alice.branch("feature/b", &base).await;
    alice.save("feature/b", "three.txt", &base).await;
    let second = alice.open_pull("Feature B", "feature/b", "main").await;
    let url = format!("/api/v3/repos/alice/proj/pulls/{number}");
    let (status, _, detail) = request(&router, "pulls/get", &url, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["head"]["sha"], head);
    assert_eq!(detail["head"]["label"], "alice:feature/a");
    assert_eq!(detail["base"]["repo"]["full_name"], "alice/proj");
    assert_eq!(detail["commits"], 2);
    assert_eq!(detail["changed_files"], 2);
    assert_eq!(detail["additions"], 2);
    assert!(detail["url"].as_str().unwrap().ends_with(&url));
    assert_eq!(detail["user"]["id"], alice_id);
    let (status, headers, list) = request(
        &router,
        "pulls/list",
        "/api/v3/repos/alice/proj/pulls?per_page=1",
        Some(&reader),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list[0]["number"], second["number"]);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (_, _, list) = request(
        &router,
        "pulls/list",
        "/api/v3/repos/alice/proj/pulls?per_page=1&page=2",
        None,
    )
    .await;
    assert_eq!(list[0]["number"], number);
    for query in [
        "head=alice:feature%2Fa",
        "sort=created&direction=asc&base=main&per_page=1",
    ] {
        let (status, _, list) = request(
            &router,
            "pulls/list",
            &format!("/api/v3/repos/alice/proj/pulls?{query}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{list}");
        assert_eq!(list[0]["number"], number);
    }
    for query in [
        "state=merged",
        "sort=nope",
        "direction=sideways",
        "head=feature",
    ] {
        let (status, _, body) = request(
            &router,
            "pulls/list",
            &format!("/api/v3/repos/alice/proj/pulls?{query}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    let (status, headers, commits) = request(
        &router,
        "pulls/list-commits",
        &format!("{url}/commits?per_page=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{commits}");
    assert_eq!(commits[0]["sha"], first);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (_, _, commits) = request(
        &router,
        "pulls/list-commits",
        &format!("{url}/commits?per_page=1&page=2"),
        None,
    )
    .await;
    assert_eq!(commits[0]["sha"], head);
    let (status, headers, files) = request(
        &router,
        "pulls/list-files",
        &format!("{url}/files?per_page=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{files}");
    assert_eq!(files[0]["filename"], "one.txt");
    assert_eq!(files[0]["status"], "added");
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (_, _, files) = request(
        &router,
        "pulls/list-files",
        &format!("{url}/files?per_page=1&page=2"),
        None,
    )
    .await;
    assert_eq!(files[0]["filename"], "two.txt");
    // `gh pr diff` asks for the diff and patch media types.
    let (status, diff) = text(&router, &url, "application/vnd.github.v3.diff").await;
    assert_eq!(status, StatusCode::OK, "{diff}");
    assert!(
        diff.starts_with("diff --git a/one.txt b/one.txt\n"),
        "{diff}"
    );
    assert!(diff.contains("--- /dev/null\n+++ b/one.txt\n"), "{diff}");
    assert!(diff.contains("diff --git a/two.txt b/two.txt\n"), "{diff}");
    let (status, patch) = text(&router, &url, "application/vnd.github.patch").await;
    assert_eq!(status, StatusCode::OK, "{patch}");
    // The patch is the commit series, oldest first, as `git am` applies it.
    assert!(patch.starts_with(&format!("From {first} ")), "{patch}");
    assert!(patch.contains(&format!("\nFrom {head} ")), "{patch}");
    assert!(patch.contains("\nSubject: [PATCH] "), "{patch}");
    assert!(
        patch.contains("\n---\ndiff --git a/one.txt b/one.txt\n"),
        "{patch}"
    );
    // A comparison answers the same media types.
    let compare = "/api/v3/repos/alice/proj/compare/main...feature/a";
    let (status, compared) = text(&router, compare, "application/vnd.github.diff").await;
    assert_eq!(status, StatusCode::OK, "{compared}");
    assert!(
        compared.starts_with("diff --git a/one.txt b/one.txt\n"),
        "{compared}"
    );
    let (status, compared) = text(&router, compare, "application/vnd.github.patch").await;
    assert_eq!(status, StatusCode::OK, "{compared}");
    assert_eq!(compared, patch);
    // GitHub's `.diff` and `.patch` page links serve the same text.
    let (status, page) = text(&router, &format!("/alice/proj/pull/{number}.diff"), "*/*").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page, diff);
    let (status, page) = text(&router, &format!("/alice/proj/pull/{number}.patch"), "*/*").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page, patch);
    let (status, commit_diff) = text(
        &router,
        &format!("/api/v3/repos/alice/proj/commits/{head}"),
        "application/vnd.github.diff",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{commit_diff}");
    assert!(
        commit_diff.starts_with("diff --git a/two.txt b/two.txt\n"),
        "{commit_diff}"
    );
    let (status, _, _) = request(
        &router,
        "pulls/check-if-merged",
        &format!("{url}/merge"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for token in [&reader, &other_writer] {
        let (status, _, body) = exchange(
            &router,
            "pulls/merge",
            "PUT",
            &format!("{url}/merge"),
            Some(token),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    let (status, _, body) = exchange(
        &router,
        "pulls/merge",
        "PUT",
        &format!("{url}/merge"),
        Some(&writer),
        json!({"sha": "0".repeat(40)}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    for input in [
        json!({"merge_method": "fast-forward"}),
        json!({"commit_title": "bad\0message"}),
        json!({"commit_message": "x".repeat(8193)}),
    ] {
        let (status, _, body) = exchange(
            &router,
            "pulls/merge",
            "PUT",
            &format!("{url}/merge"),
            Some(&writer),
            input,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(!body["errors"].as_array().unwrap().is_empty());
    }
    let (status, _, body) = exchange(&router, "pulls/merge", "PUT", &format!("{url}/merge"), Some(&writer), json!({"sha": head, "commit_title": "Custom merge", "commit_message": "Custom description"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["merged"], true);
    let repo = codeyard_db_repos::repositories::records::get_repository(f.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let store = codeyard_repos_core::repositories::store::repo_store(&repo, None);
    let commit = store.commit(body["sha"].as_str().unwrap()).await.unwrap();
    assert_eq!(commit.message, "Custom merge\n\nCustom description");
    assert_eq!(commit.parents.len(), 2);
    let (status, _, again) = exchange(
        &router,
        "pulls/merge",
        "PUT",
        &format!("{url}/merge"),
        Some(&writer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["sha"], body["sha"]);
    let (status, _, _) = request(
        &router,
        "pulls/check-if-merged",
        &format!("{url}/merge"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, _, merged) = request(&router, "pulls/get", &url, None).await;
    assert_eq!(merged["state"], "closed");
    assert_eq!(merged["merged"], true);
    assert_eq!(merged["merged_by"]["id"], alice_id);
    let (_, _, closed) = request(
        &router,
        "pulls/list",
        "/api/v3/repos/alice/proj/pulls?state=closed",
        None,
    )
    .await;
    assert_eq!(closed.as_array().unwrap().len(), 1);
    assert_eq!(closed[0]["number"], number);
    let second_id = second["id"].as_i64().unwrap();
    let second_url = format!("/api/v3/repos/alice/proj/pulls/{}/merge", second["number"]);
    f.execute(
        "UPDATE pull_requests SET is_draft=TRUE WHERE id=$1",
        &[second_id],
    )
    .await;
    let (status, _, body) = exchange(
        &router,
        "pulls/merge",
        "PUT",
        &second_url,
        Some(&writer),
        json!({"merge_method": "squash"}),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    f.execute(
        "UPDATE pull_requests SET is_draft=FALSE WHERE id=$1",
        &[second_id],
    )
    .await;
    let (status, _, body) = exchange(&router, "pulls/merge", "PUT", &second_url, Some(&writer), json!({"merge_method": "squash", "commit_title": "Squashed feature B", "commit_message": "Squash description"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let commit = store.commit(body["sha"].as_str().unwrap()).await.unwrap();
    assert_eq!(commit.message, "Squashed feature B\n\nSquash description");
    assert_eq!(commit.parents.len(), 1);
    alice.branch("rebased", &commit.sha).await;
    alice.save("rebased", "four.txt", &commit.sha).await;
    let rebased = alice.open_pull("Feature C", "rebased", "main").await;
    let url = format!("/api/v3/repos/alice/proj/pulls/{}/merge", rebased["number"]);
    let (status, _, body) = exchange(
        &router,
        "pulls/merge",
        "PUT",
        &url,
        Some(&writer),
        json!({"merge_method": "rebase"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let commit = store.commit(body["sha"].as_str().unwrap()).await.unwrap();
    assert_eq!(commit.message, "Add four.txt");
    assert_eq!(commit.parents.len(), 1);
    for token in [None, Some(outsider.as_str()), Some(reader.as_str())] {
        let (status, _, _) = request(
            &router,
            "pulls/get",
            "/api/v3/repos/alice/secret/pulls/1",
            token,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (status, _, _) = request(
        &router,
        "pulls/get",
        "/api/v3/repos/alice/proj/pulls/999",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let repo = codeyard_db_repos::repositories::records::get_repository(f.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let path = repo.disk_path.clone();
    let (first, last) = tokio::task::spawn_blocking(move || {
        let git = git2::Repository::open_bare(path).unwrap();
        let mut parent = git
            .find_commit(git2::Oid::from_str(&base).unwrap())
            .unwrap();
        let tree = parent.tree().unwrap();
        let signature = git2::Signature::now("alice", "alice@users.noreply").unwrap();
        let mut first = String::new();
        for index in 0..251 {
            let id = git
                .commit(
                    None,
                    &signature,
                    &signature,
                    &format!("Commit {index}"),
                    &tree,
                    &[&parent],
                )
                .unwrap();
            if index == 0 {
                first = id.to_string();
            }
            parent = git.find_commit(id).unwrap();
        }
        git.reference("refs/heads/many", parent.id(), true, "fixture")
            .unwrap();
        (first, parent.id().to_string())
    })
    .await
    .unwrap();
    let many = alice.open_pull("Many commits", "many", "main").await;
    let url = format!("/api/v3/repos/alice/proj/pulls/{}", many["number"]);
    let (_, _, detail) = request(&router, "pulls/get", &url, None).await;
    assert_eq!(detail["commits"], 251);
    let (_, headers, commits) = request(
        &router,
        "pulls/list-commits",
        &format!("{url}/commits?per_page=100"),
        None,
    )
    .await;
    assert_eq!(commits.as_array().unwrap().len(), 100);
    assert_eq!(commits[0]["sha"], first);
    assert!(headers.contains_key("link"));
    let (_, headers, commits) = request(
        &router,
        "pulls/list-commits",
        &format!("{url}/commits?per_page=100&page=3"),
        None,
    )
    .await;
    assert_eq!(commits.as_array().unwrap().len(), 50);
    assert!(commits
        .as_array()
        .unwrap()
        .iter()
        .all(|commit| commit["sha"] != last));
    assert!(!headers["link"].to_str().unwrap().contains("rel=\"next\""));
    f.finish().await;
}

#[tokio::test]
async fn sqlite_github_pull_reads() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_pull_reads() {
    contract(true).await;
}
