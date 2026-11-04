//! GitHub REST documents through the real router on both database engines.
#![cfg(not(target_arch = "wasm32"))]
mod activity;
mod autolinks;
mod code_owners;
mod community;
mod copy;
mod dependencies;
mod dispatch;
mod immutable_releases;
mod native_invitations;
mod patch;
mod release_notes;
mod repository_action_permissions;
mod teams;
mod traffic;
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use codeyard_storage::ObjectStore;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

mod access_restrictions;
mod advisories;
mod branch_admission;
mod code_scanning;
mod code_security;
mod commit_comments;
mod commit_pulls;
mod comparison;
mod dependabot_alerts;
mod dependency_graph;
mod dependency_inventory;
mod merges;
mod migrations;
mod oidc;
mod pages;
mod protection;
mod push_teams;
mod review_protection;
mod secret_scanning;
mod verification;

async fn request(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-github-api-version", "2022-11-28");
    if let Some(token) = token {
        request = request.header("authorization", token);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let value = if status == StatusCode::NO_CONTENT || method == "HEAD" {
        assert!(
            bytes.is_empty(),
            "{status} {method} responses must not have a body"
        );
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("{status}: {}", String::from_utf8_lossy(&bytes)))
    };
    (status, headers, value)
}

async fn git_objects(
    router: &Router,
    base: &str,
    writer: &str,
    reader: &str,
    outsider: &str,
    tip: &str,
) {
    let (status, _, blob) = request(
        router,
        "POST",
        &format!("{base}/git/blobs"),
        Some(writer),
        json!({"content":"AAEC/w==", "encoding":"base64"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{blob}");
    assert_github_schema("git/create-blob", 201, &blob);
    let blob_sha = blob["sha"].as_str().unwrap();
    let (status, _, body) = request(
        router,
        "GET",
        &format!("{base}/git/blobs/{blob_sha}"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("git/get-blob", 200, &body);
    assert_eq!(body["content"], "AAEC/w==");
    let (status, _, utf8) = request(
        router,
        "POST",
        &format!("{base}/git/blobs"),
        Some(writer),
        json!({"content":"hello 🦀"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{utf8}");
    assert_github_schema("git/create-blob", 201, &utf8);
    let (status, _, tree) = request(
        router,
        "POST",
        &format!("{base}/git/trees"),
        Some(writer),
        json!({"tree":[
            {"path":"nested/binary", "mode":"100644", "type":"blob", "sha":blob_sha},
            {"path":"script", "mode":"100755", "type":"blob", "content":"#!/bin/sh\n"},
            {"path":"link", "mode":"120000", "type":"blob", "content":"script"},
            {"path":"submodule", "mode":"160000", "type":"commit", "sha":tip}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{tree}");
    assert_github_schema("git/create-tree", 201, &tree);
    let tree_sha = tree["sha"].as_str().unwrap();
    let (status, _, body) = request(
        router,
        "GET",
        &format!("{base}/git/trees/{tree_sha}?recursive=false"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("git/get-tree", 200, &body);
    assert_eq!(body["truncated"], false);
    assert_eq!(body["tree"].as_array().unwrap().len(), 5);
    assert!(body["tree"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["path"] == "nested/binary" && entry["sha"] == blob_sha));
    let (status, _, amended) = request(router, "POST", &format!("{base}/git/trees"), Some(writer), json!({"base_tree":tree_sha,"tree":[{"path":"script", "mode":"100755", "type":"blob", "sha":null}]})).await;
    assert_eq!(status, StatusCode::CREATED, "{amended}");
    assert_github_schema("git/create-tree", 201, &amended);
    assert_eq!(amended["tree"].as_array().unwrap().len(), 3);
    let identity =
        json!({"name":"Robot", "email":"robot@example.test", "date":"2026-01-02T03:04:05+06:00"});
    let (status, _, commit) = request(router, "POST", &format!("{base}/git/commits"), Some(writer), json!({"message":"Detached commit", "tree":tree_sha, "parents":[tip], "author":identity, "committer":identity})).await;
    assert_eq!(status, StatusCode::CREATED, "{commit}");
    assert_github_schema("git/create-commit", 201, &commit);
    let sha = commit["sha"].as_str().unwrap();
    assert_eq!(commit["tree"]["sha"], tree_sha);
    assert_eq!(commit["parents"][0]["sha"], tip);
    assert_eq!(commit["author"]["name"], "Robot");
    assert_eq!(commit["verification"]["reason"], "unsigned");
    let (status, _, root_commit) = request(
        router,
        "POST",
        &format!("{base}/git/commits"),
        Some(writer),
        json!({"message":"Root commit", "tree":tree_sha}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{root_commit}");
    assert_github_schema("git/create-commit", 201, &root_commit);
    assert_eq!(root_commit["parents"], json!([]));
    let (status, _, body) = request(
        router,
        "GET",
        &format!("{base}/git/commits/{sha}"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("git/get-commit", 200, &body);
    assert_eq!(body, commit);
    let (status, _, tag) = request(router, "POST", &format!("{base}/git/tags"), Some(writer), json!({"tag":"v1", "message":"A release", "object":sha, "type":"commit", "tagger":identity})).await;
    assert_eq!(status, StatusCode::CREATED, "{tag}");
    assert_github_schema("git/create-tag", 201, &tag);
    let tag_sha = tag["sha"].as_str().unwrap();
    let (status, _, body) = request(
        router,
        "GET",
        &format!("{base}/git/tags/{tag_sha}"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("git/get-tag", 200, &body);
    assert_eq!(body, tag);
    let (status, _, body) = request(
        router,
        "GET",
        &format!("{base}/git/ref/heads/main"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("git/get-ref", 200, &body);
    assert_eq!(body["object"]["sha"], tip);
    for (suffix, target, kind) in [
        ("tags/v1", tag_sha, "tag"),
        ("tags/blob", blob_sha, "blob"),
        ("tags/tree-snapshot", tree_sha, "tree"),
        ("heads/object-test", tip, "commit"),
    ] {
        let (status, _, body) = request(
            router,
            "POST",
            &format!("{base}/git/refs"),
            Some(writer),
            json!({"ref":format!("refs/{suffix}"), "sha":target}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_github_schema("git/create-ref", 201, &body);
        assert_eq!(body["object"]["sha"], target);
        assert_eq!(body["object"]["type"], kind);
        let (status, _, body) = request(
            router,
            "GET",
            &format!("{base}/git/ref/{suffix}"),
            Some(reader),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_github_schema("git/get-ref", 200, &body);
        assert_eq!(body["object"]["type"], kind);
    }
    let (status, _, alias) = request(
        router,
        "GET",
        &format!("{base}/git/trees/tree-snapshot"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{alias}");
    assert_github_schema("git/get-tree", 200, &alias);
    assert_eq!(alias["sha"], tree_sha);
    let update_path = format!("{base}/git/refs/heads/object-test");
    for (token, expected) in [
        (Some(reader), StatusCode::FORBIDDEN),
        (Some(outsider), StatusCode::NOT_FOUND),
    ] {
        let (status, _, body) =
            request(router, "PATCH", &update_path, token, json!({"sha":sha})).await;
        assert_eq!(status, expected, "{body}");
    }
    let (status, _, body) = request(
        router,
        "PATCH",
        &update_path,
        Some(writer),
        json!({"sha":sha}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("git/update-ref", 200, &body);
    assert_eq!(body["object"]["sha"], sha);
    let (status, _, body) = request(
        router,
        "PATCH",
        &update_path,
        Some(writer),
        json!({"sha":tip}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_github_schema("git/update-ref", 422, &body);
    let (status, _, body) = request(
        router,
        "PATCH",
        &update_path,
        Some(writer),
        json!({"sha":tip, "force":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("git/update-ref", 200, &body);
    assert_eq!(body["object"]["sha"], tip);
    let (status, _, body) = request(
        router,
        "PATCH",
        &update_path,
        Some(writer),
        json!({"sha":blob_sha, "force":true}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_github_schema("git/update-ref", 422, &body);
    for suffix in [
        "tags/v1",
        "tags/blob",
        "tags/tree-snapshot",
        "heads/object-test",
    ] {
        let (status, _, body) = request(
            router,
            "DELETE",
            &format!("{base}/git/refs/{suffix}"),
            Some(writer),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    let entries: Vec<_> = (0..1_000).map(|number| json!({"path":format!("wide/{number:04}"), "mode":"100644", "type":"blob", "sha":blob_sha})).collect();
    let (status, _, large) = request(
        router,
        "POST",
        &format!("{base}/git/trees"),
        Some(writer),
        json!({"tree":entries}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{large}");
    assert_github_schema("git/create-tree", 201, &large);
    let (status, _, truncated) = request(
        router,
        "GET",
        &format!(
            "{base}/git/trees/{}?recursive=1",
            large["sha"].as_str().unwrap()
        ),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{truncated}");
    assert_github_schema("git/get-tree", 200, &truncated);
    assert_eq!(truncated["truncated"], true);
    assert_eq!(truncated["tree"].as_array().unwrap().len(), 1_000);
    let missing = "f".repeat(40);
    for (suffix, operation) in [
        (format!("trees/{missing}"), "git/get-tree"),
        (format!("tags/{missing}"), "git/get-tag"),
        (format!("blobs/{missing}"), "git/get-blob"),
    ] {
        let (status, _, body) = request(
            router,
            "GET",
            &format!("{base}/git/{suffix}"),
            Some(reader),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_github_schema(operation, 404, &body);
    }
    for (suffix, operation) in [
        (format!("trees/{tree_sha}"), "git/get-tree"),
        (format!("tags/{tag_sha}"), "git/get-tag"),
        (format!("blobs/{blob_sha}"), "git/get-blob"),
    ] {
        let (status, _, body) = request(
            router,
            "GET",
            &format!("{base}/git/{suffix}"),
            Some(outsider),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_github_schema(operation, 404, &body);
    }
    for (suffix, input) in [
        ("blobs", json!({"content":"hello"})),
        ("trees", json!({"tree":[]})),
        ("commits", json!({"message":"denied", "tree":tree_sha})),
        (
            "tags",
            json!({"tag":"v2", "message":"denied", "object":sha, "type":"commit"}),
        ),
    ] {
        for (token, expected) in [
            (Some(reader), StatusCode::FORBIDDEN),
            (Some(outsider), StatusCode::NOT_FOUND),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            let (status, _, body) = request(
                router,
                "POST",
                &format!("{base}/git/{suffix}"),
                token,
                input.clone(),
            )
            .await;
            assert_eq!(status, expected, "{suffix}: {body}");
        }
    }
    for (suffix, operation, input) in [
        (
            "blobs",
            "git/create-blob",
            json!({"encoding":"base64", "content":"invalid!"}),
        ),
        (
            "trees",
            "git/create-tree",
            json!({"tree":[{"path":"../escape", "type":"blob", "mode":"100644", "content":"x"}]}),
        ),
        (
            "trees",
            "git/create-tree",
            json!({"tree":[{"path":"mismatch", "type":"tree", "mode":"100644", "sha":tree_sha}]}),
        ),
        (
            "commits",
            "git/create-commit",
            json!({"message":"invalid", "tree":blob_sha}),
        ),
        (
            "tags",
            "git/create-tag",
            json!({"tag":"v2", "message":"invalid", "object":blob_sha, "type":"commit"}),
        ),
    ] {
        let (status, _, body) = request(
            router,
            "POST",
            &format!("{base}/git/{suffix}"),
            Some(writer),
            input,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{suffix}: {body}");
        assert_github_schema(operation, 422, &body);
    }
}

async fn large_blobs(router: &Router, base: &str, writer: &str) {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use codeyard_git::objects::MAX_BLOB_WRITE_BYTES;
    for (size, base64) in [
        (5 * 1024 * 1024, false),
        (MAX_BLOB_WRITE_BYTES, true),
        (MAX_BLOB_WRITE_BYTES + 1, true),
    ] {
        let bytes = vec![b'x'; size];
        let sha = git2::Oid::hash_object(git2::ObjectType::Blob, &bytes)
            .unwrap()
            .to_string();
        let input = if base64 {
            json!({"content": STANDARD.encode(&bytes), "encoding":"base64"})
        } else {
            json!({"content":String::from_utf8(bytes).unwrap(), "encoding":"utf-8"})
        };
        let (status, _, body) = request(
            router,
            "POST",
            &format!("{base}/git/blobs"),
            Some(writer),
            input,
        )
        .await;
        if size > MAX_BLOB_WRITE_BYTES {
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_github_schema("git/create-blob", 422, &body);
        } else {
            assert_eq!(status, StatusCode::CREATED, "{body}");
            assert_github_schema("git/create-blob", 201, &body);
            assert_eq!(body["sha"], sha);
        }
    }
}

async fn status_history(router: &Router, base: &str, writer: &str, reader: &str, sha: &str) {
    let mut saved: Vec<Value> = Vec::new();
    for (context, state) in [
        ("default", "pending"),
        ("default", "failure"),
        ("default", "success"),
        ("lint", "pending"),
        // Contexts compare without case, as on GitHub.
        ("Lint", "success"),
    ] {
        let (status, _, body) = request(
            router,
            "POST",
            &format!("{base}/statuses/{sha}"),
            Some(writer),
            json!({"context":context, "state":state}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_github_schema("repos/create-commit-status", 201, &body);
        if let Some(previous) = saved.last() {
            assert!(body["id"].as_i64().unwrap() > previous["id"].as_i64().unwrap());
        }
        saved.push(body);
    }
    let (status, _, body) = request(
        router,
        "GET",
        &format!("{base}/commits/{sha}/status"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("repos/get-combined-status-for-ref", 200, &body);
    assert_eq!(body["state"], "success");
    assert_eq!(body["total_count"], 2);
    assert_eq!(saved[4]["context"], "lint");
    assert_eq!(body["statuses"], json!([saved[4], saved[2]]));
    let mut history = Vec::new();
    for page in 1..=3 {
        let (status, headers, body) = request(
            router,
            "GET",
            &format!("{base}/commits/{sha}/statuses?per_page=2&page={page}"),
            Some(reader),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_github_schema("repos/list-commit-statuses-for-ref", 200, &body);
        if page < 3 {
            assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
        }
        history.extend(body.as_array().unwrap().iter().cloned());
    }
    saved.reverse();
    assert_eq!(history, saved);
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repository_row = codeyard_app::repositories::lifecycle::provision(
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
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let session = Client::login(&router, f.pool(), alice).await;
    let other = Client::login(&router, f.pool(), bob).await;
    let token = |name, scope| TokenInput {
        name,
        scope,
        expires_at: None,
    };
    let write = tokens::create(
        f.pool(),
        alice,
        Some(&session.session),
        token("automation", "write"),
    )
    .await
    .unwrap();
    let read = tokens::create(
        f.pool(),
        alice,
        Some(&session.session),
        token("reader", "read"),
    )
    .await
    .unwrap();
    let other = tokens::create(f.pool(), bob, Some(&other.session), token("other", "write"))
        .await
        .unwrap();
    let bearer = format!("Bearer {}", write.value);
    let reader = format!("token {}", read.value);
    let other = format!("token {}", other.value);
    let base = "/api/v3/repos/alice/proj";

    let (status, headers, user) =
        request(&router, "GET", "/api/v3/user", Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{user}");
    crate::support::github_spec::assert_github_schema("users/get-authenticated", 200, &user);
    assert_eq!(headers["x-github-api-version-selected"], "2022-11-28");
    assert_eq!(user["login"], "alice");
    assert_eq!(user["type"], "User");
    assert_eq!(user["id"], alice);
    assert!(user.get("password_hash").is_none());
    let (status, _, body) = request(&router, "GET", "/api/v3/user", None, Value::Null).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body,
        json!({"message":"Requires authentication", "documentation_url":"https://docs.github.com/en/rest", "status":"401"})
    );
    for auth in [None, Some(other.as_str())] {
        let (status, _, body) = request(&router, "GET", base, auth, Value::Null).await;
        assert!(
            matches!(status, StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED),
            "{status}: {body}"
        );
    }
    let (status, _, repository) = request(&router, "GET", base, Some(&bearer), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{repository}");
    assert_github_schema("repos/get", status.as_u16(), &repository);
    assert_eq!(repository["full_name"], "alice/proj");
    assert_eq!(repository["private"], true);
    assert_eq!(repository["owner"]["login"], "alice");
    assert!(repository["url"].as_str().unwrap().ends_with(base));

    let file = format!("{base}/contents/docs/hello.txt");
    let create = json!({"message":"Create greeting", "content":"aGVsbG8K"});
    let (status, _, body) = request(&router, "PUT", &file, Some(&reader), create.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _, saved) = request(&router, "PUT", &file, Some(&bearer), create).await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    assert_github_schema(
        "repos/create-or-update-file-contents",
        status.as_u16(),
        &saved,
    );
    let blob = saved["content"]["sha"].as_str().unwrap();
    let commit = saved["commit"]["sha"].as_str().unwrap();
    assert_eq!(blob.len(), 40);
    assert_ne!(blob, commit);
    let (status, _, contents) = request(&router, "GET", &file, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{contents}");
    assert_github_schema("repos/get-content", status.as_u16(), &contents);
    // The documented contents response; volatile IDs and the configured origin are explicit.
    let url = contents["url"].as_str().unwrap();
    let html = contents["html_url"].as_str().unwrap();
    let git = contents["git_url"].as_str().unwrap();
    let download = contents["download_url"].as_str().unwrap();
    assert!(url.ends_with("/api/v3/repos/alice/proj/contents/docs/hello.txt?ref=main"));
    assert!(html.ends_with("/alice/proj/blob/main/docs/hello.txt"));
    assert!(git.ends_with(&format!("/git/blobs/{blob}")));
    // A bearer client can follow the download link, pinned to the commit that was read.
    assert!(download.ends_with(&format!(
        "/api/v3/repos/alice/proj/contents/docs/hello.txt?ref={commit}&raw=1"
    )));
    assert_eq!(
        contents,
        json!({
            "type":"file", "encoding":"base64", "size":6, "name":"hello.txt", "path":"docs/hello.txt",
            "content":"aGVsbG8K", "sha":blob, "url":url, "html_url":html, "git_url":git, "download_url":download,
            "_links":{"self":url,"git":git,"html":html},
        })
    );
    let (status, _, directory) = request(
        &router,
        "GET",
        &format!("{base}/contents/docs"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{directory}");
    assert_github_schema("repos/get-content", status.as_u16(), &directory);
    assert_eq!(directory[0]["sha"], blob);
    let (status, _, root) = request(
        &router,
        "GET",
        &format!("{base}/contents"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{root}");
    assert_github_schema("repos/get-content", status.as_u16(), &root);
    assert_eq!(root[0]["type"], "dir");
    assert_eq!(root[0]["sha"].as_str().unwrap().len(), 40);

    for payload in [
        json!({"message":"Overwrite", "content":"bmV3Cg=="}),
        json!({"message":"Overwrite", "content":"bmV3Cg==", "sha":"a".repeat(40)}),
    ] {
        let (status, _, body) = request(&router, "PUT", &file, Some(&bearer), payload).await;
        assert!(
            matches!(
                status,
                StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY
            ),
            "{status}: {body}"
        );
        assert_github_schema(
            "repos/create-or-update-file-contents",
            status.as_u16(),
            &body,
        );
    }
    let (status, _, saved) = request(
        &router,
        "PUT",
        &file,
        Some(&bearer),
        json!({"message":"Update greeting", "content":"bmV3Cg==", "sha":blob}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_github_schema(
        "repos/create-or-update-file-contents",
        status.as_u16(),
        &saved,
    );
    let tip = saved["commit"]["sha"].as_str().unwrap();
    let (status, _, git_commit) = request(
        &router,
        "GET",
        &format!("{base}/git/commits/{tip}"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{git_commit}");
    assert_github_schema("git/get-commit", 200, &git_commit);
    assert_eq!(git_commit["sha"], tip);
    assert_eq!(git_commit["tree"]["sha"].as_str().unwrap().len(), 40);
    assert_eq!(git_commit["parents"][0]["sha"], commit);
    assert_eq!(git_commit["author"]["name"], "alice");
    assert_eq!(git_commit["author"]["email"], "alice@users.noreply");
    let (status, _, detail) = request(
        &router,
        "GET",
        &format!("{base}/commits/main"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_github_schema("repos/get-commit", 200, &detail);
    assert_eq!(detail["sha"], tip);
    assert_eq!(
        detail["stats"],
        json!({"total":2,"additions":1,"deletions":1})
    );
    assert_eq!(detail["files"][0]["filename"], "docs/hello.txt");
    let (status, headers, commits) = request(
        &router,
        "GET",
        &format!("{base}/commits?per_page=1"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{commits}");
    assert_github_schema("repos/list-commits", 200, &commits);
    assert_eq!(commits[0]["sha"], tip);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (status, _, commits) = request(
        &router,
        "GET",
        &format!("{base}/commits?per_page=100"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{commits}");
    assert_github_schema("repos/list-commits", 200, &commits);
    assert_eq!(commits.as_array().unwrap().len(), 2);
    let blob_id = saved["content"]["sha"].as_str().unwrap();
    let (status, _, blob) = request(
        &router,
        "GET",
        &format!("{base}/git/blobs/{blob_id}"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{blob}");
    assert_github_schema("git/get-blob", 200, &blob);
    assert_eq!(
        blob,
        json!({"sha":blob_id,"node_id":blob["node_id"],"url":format!("{}/git/blobs/{blob_id}", repository["url"].as_str().unwrap()),"size":4,"encoding":"base64","content":"bmV3Cg=="})
    );

    git_objects(&router, base, &bearer, &reader, &other, tip).await;
    large_blobs(&router, base, &bearer).await;
    verification::run(&router, base, &bearer, tip).await;
    status_history(&router, base, &bearer, &reader, commit).await;
    protection::run(
        &router,
        (f.pool(), repository_row.id, alice),
        base,
        (&bearer, &reader, &other),
        tip,
    )
    .await;

    access_restrictions::run(
        &router,
        (f.pool(), repository_row.id, alice, bob),
        base,
        (&bearer, &reader, &other),
        tip,
    )
    .await;

    push_teams::run(
        &router,
        (f.pool(), alice, bob),
        (&bearer, &reader, &other),
        &session,
    )
    .await;

    review_protection::run(
        &router,
        (f.pool(), repository_row.id, alice),
        base,
        (&bearer, &reader, &other),
        tip,
    )
    .await;

    merges::run(
        &router,
        (f.pool(), repository_row.id, alice),
        base,
        (&bearer, &reader, &other),
        tip,
        &session,
    )
    .await;

    comparison::run(
        &router,
        (f.pool(), repository_row.id, alice, bob),
        base,
        (&bearer, &reader, &other),
        tip,
        &session,
    )
    .await;

    commit_comments::run(
        &router,
        (f.pool(), repository_row.id, alice, bob),
        base,
        (&bearer, &reader, &other),
        tip,
        &session,
    )
    .await;

    let combined = format!("{base}/commits/main/status");
    let (status, _, empty) = request(&router, "GET", &combined, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_github_schema("repos/get-combined-status-for-ref", 200, &empty);
    assert_eq!(empty["state"], "pending");
    assert_eq!(empty["total_count"], 0);
    let statuses = format!("{base}/statuses/{tip}");
    let (status, _, denied) = request(
        &router,
        "POST",
        &statuses,
        Some(&reader),
        json!({"state":"success"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    let (status, _, invalid) = request(
        &router,
        "POST",
        &statuses,
        Some(&bearer),
        json!({"state":"completed"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
    assert_eq!(invalid["errors"][0]["code"], "custom");
    let (status, _, denied) = request(
        &router,
        "POST",
        &statuses,
        Some(&bearer),
        json!({"state":"success", "context":"gitserv/workflow/spoof.yml"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    let (status, _, reported) = request(
        &router,
        "POST",
        &statuses,
        Some(&bearer),
        json!({"state":"success", "description":"All checks passed"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reported}");
    assert_github_schema("repos/create-commit-status", 201, &reported);
    let reported_url = reported["url"].as_str().unwrap();
    assert!(reported_url.ends_with(&format!("/statuses/{tip}")));
    assert_eq!(
        reported,
        json!({
            "id":reported["id"], "node_id":reported["node_id"], "avatar_url":reported["avatar_url"], "url":reported_url,"state":"success","context":"default",
            "description":"All checks passed","target_url":null,
            "creator":codeyard_webhooks::payloads::person(&codeyard_webhooks::payloads::Person { id:alice, login:"alice".into(), site_admin:false }),
            "created_at":reported["created_at"],"updated_at":reported["updated_at"],
        })
    );
    let (status, _, result) = request(&router, "GET", &combined, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_github_schema("repos/get-combined-status-for-ref", 200, &result);
    assert_eq!(reported["creator"]["id"], alice);
    assert_eq!(reported["creator"]["login"], "alice");
    assert_eq!(result["state"], "success");
    assert_eq!(result["statuses"][0], reported);
    let (status, _, reported) = request(
        &router,
        "POST",
        &statuses,
        Some(&bearer),
        json!({"state":"failure", "context":"lint"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reported}");
    let (status, headers, result) = request(
        &router,
        "GET",
        &format!("{combined}?per_page=1"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_github_schema("repos/create-commit-status", 201, &reported);
    assert_github_schema("repos/get-combined-status-for-ref", 200, &result);
    assert_eq!(result["state"], "failure");
    assert_eq!(result["total_count"], 2);
    assert_eq!(result["statuses"].as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    for branch in ["one", "two"] {
        let (status, _, body) = request(
            &router,
            "POST",
            &format!("{base}/git/refs"),
            Some(&bearer),
            json!({"ref":format!("refs/heads/{branch}"),"sha":tip}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_github_schema("git/create-ref", 201, &body);
        assert_eq!(body["ref"], format!("refs/heads/{branch}"));
        assert_eq!(body["object"]["type"], "commit");
    }
    let (status, _, body) = request(
        &router,
        "GET",
        &format!("{base}/git/ref/heads/main"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("git/get-ref", 200, &body);
    assert_eq!(body["object"]["sha"], tip);
    let (status, headers, page) = request(
        &router,
        "GET",
        &format!("{base}/git/refs?per_page=2"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_github_schema("git/list-matching-refs", 200, &page);
    assert_eq!(page.as_array().unwrap().len(), 2);
    assert!(headers["link"]
        .to_str()
        .unwrap()
        .contains("page=2&per_page=2>"));
    let (status, headers, page) = request(
        &router,
        "GET",
        &format!("{base}/git/refs?per_page=2&page=2"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_github_schema("git/list-matching-refs", 200, &page);
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert!(!headers["link"].to_str().unwrap().contains("rel=\"next\""));
    assert!(headers["link"].to_str().unwrap().contains("rel=\"prev\""));
    let (status, _, _) = request(
        &router,
        "GET",
        &format!("{base}/git/refs?page=0"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, headers, branches) = request(
        &router,
        "GET",
        &format!("{base}/branches?per_page=1"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{branches}");
    assert_github_schema("repos/list-branches", 200, &branches);
    assert_eq!(branches[0]["name"], "main");
    assert_eq!(branches[0]["protected"], false);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (status, _, branch) = request(
        &router,
        "GET",
        &format!("{base}/branches/main"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{branch}");
    assert_github_schema("repos/get-branch", 200, &branch);
    assert_eq!(branch["commit"]["sha"], tip);
    let (status, _, branches) = request(
        &router,
        "GET",
        &format!("{base}/commits/{tip}/branches-where-head"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{branches}");
    assert_github_schema("repos/list-branches-for-head-commit", 200, &branches);
    assert_eq!(branches.as_array().unwrap().len(), 3);
    codeyard_db_repos::repositories::protection::upsert_branch_protection_rule(
        f.pool(),
        codeyard_db_repos::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: alice,
            repo_id: repository_row.id,
            pattern: "two",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (status, _, branches) = request(
        &router,
        "GET",
        &format!("{base}/branches?protected=true"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{branches}");
    assert_github_schema("repos/list-branches", 200, &branches);
    assert_eq!(branches.as_array().unwrap().len(), 1);
    assert_eq!(branches[0]["name"], "two");
    let (status, _, branch) = request(
        &router,
        "GET",
        &format!("{base}/branches/two"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{branch}");
    assert_github_schema("repos/get-branch", 200, &branch);
    assert_eq!(branch["protected"], true);
    let (status, _, body) = request(
        &router,
        "DELETE",
        &format!("{base}/git/refs/heads/two"),
        Some(&bearer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _, body) = request(
        &router,
        "PATCH",
        &format!("{base}/git/refs/heads/two"),
        Some(&bearer),
        json!({"sha":commit, "force":true}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    for (path, operation) in [
        ("branches/missing", "repos/get-branch"),
        ("branches/main", "repos/get-branch"),
    ] {
        let auth = if path.ends_with("missing") {
            Some(reader.as_str())
        } else {
            Some(other.as_str())
        };
        let (status, _, body) =
            request(&router, "GET", &format!("{base}/{path}"), auth, Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_github_schema(operation, 404, &body);
    }
    for (auth, expected) in [
        (Some(reader.as_str()), StatusCode::FORBIDDEN),
        (Some(other.as_str()), StatusCode::NOT_FOUND),
        (None, StatusCode::UNAUTHORIZED),
    ] {
        let (status, _, body) = request(
            &router,
            "DELETE",
            &format!("{base}/git/refs/heads/one"),
            auth,
            Value::Null,
        )
        .await;
        assert_eq!(status, expected, "{body}");
    }
    let (status, _, body) = request(
        &router,
        "DELETE",
        &format!("{base}/git/refs/heads/main"),
        Some(&bearer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_github_schema("git/delete-ref", 422, &body);
    let (status, _, body) = request(
        &router,
        "DELETE",
        &format!("{base}/git/refs/heads/one"),
        Some(&bearer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(body.is_null());
    let (status, _, body) = request(
        &router,
        "DELETE",
        &format!("{base}/git/refs/heads/one"),
        Some(&bearer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_github_schema("git/delete-ref", 422, &body);
    let (status, _, body) = request(
        &router,
        "GET",
        &format!("{base}/git/ref/heads/one"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_github_schema("git/get-ref", 404, &body);
    let (status, _, body) = request(
        &router,
        "GET",
        &format!("{base}/commits/main/statuses"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("repos/list-commit-statuses-for-ref", 200, &body);
    assert_eq!(body.as_array().unwrap().len(), 2);
    let (status, _, deleted) = request(
        &router,
        "DELETE",
        &file,
        Some(&bearer),
        json!({"message":"Delete greeting","sha":saved["content"]["sha"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_github_schema("repos/delete-file", status.as_u16(), &deleted);
    assert_eq!(deleted["content"], Value::Null);
    let (status, _, _) = request(&router, "GET", &file, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    commit_pulls::run(
        &router,
        (f.pool(), repository_row.id, alice, bob),
        base,
        (&bearer, &reader, &other),
        &session,
    )
    .await;

    f.execute(
        "UPDATE personal_access_tokens SET expires_at = '2000-01-01T00:00:00Z' WHERE id = $1",
        &[read.token.id],
    )
    .await;
    let (status, _, body) =
        request(&router, "GET", "/api/v3/user", Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["message"], "Bad credentials");

    let (_, gists) = checked(
        &router,
        "gists/list-public",
        "GET",
        "/api/v3/gists/public",
        Some(&bearer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        gists,
        json!([]),
        "Ordinary repositories must not appear as gists"
    );
    let (_, emojis) = checked(
        &router,
        "emojis/get",
        "GET",
        "/api/v3/emojis",
        Some(&bearer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(emojis["heart"].as_str().unwrap().ends_with("/emojis/heart"));
    // Unsupported products retain the GHES-style 404 rather than a fake success.
    let path = format!("{base}/codespaces");
    let (status, _, body) = request(&router, "GET", &path, Some(&bearer), Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    assert!(body["documentation_url"]
        .as_str()
        .unwrap()
        .starts_with("https://docs.github.com/rest/"));
}

async fn checked(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    input: Value,
    expected: StatusCode,
) -> (HeaderMap, Value) {
    let (status, headers, body) = request(router, method, path, token, input).await;
    assert_eq!(status, expected, "{operation}: {body}");
    if status != StatusCode::NO_CONTENT {
        assert_github_schema(operation, status.as_u16(), &body);
    }
    (headers, body)
}

async fn credential(router: &Router, database: &Database, actor: i64, scope: &str) -> String {
    let client = Client::login(router, database.pool(), actor).await;
    let token = tokens::create(
        database.pool(),
        actor,
        Some(&client.session),
        TokenInput {
            name: scope,
            scope,
            expires_at: None,
        },
    )
    .await
    .unwrap();
    format!("Bearer {}", token.value)
}

async fn hooks_contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(Arc::new(codeyard_support::secrets::SecretEncryptor::new(
            &[7; 32],
        ))),
        CiConfig::default(),
        None,
    ));
    let writer = credential(&router, &f, alice, "write").await;
    let reader = credential(&router, &f, alice, "read").await;
    let outsider = credential(&router, &f, bob, "write").await;
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"proj", "private":true, "auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let base = "/api/v3/repos/alice/proj/hooks";
    let input = json!({"name":"web", "events":["push"], "config":{"url":"https://hooks.example.test/push", "content_type":"json", "secret":"secret"}});
    checked(
        &router,
        "repos/create-webhook",
        "POST",
        base,
        Some(&reader),
        input.clone(),
        StatusCode::FORBIDDEN,
    )
    .await;
    let (_, hook) = checked(
        &router,
        "repos/create-webhook",
        "POST",
        base,
        Some(&writer),
        input,
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(hook["config"]["secret"], "********");
    let id = hook["id"].as_i64().unwrap();
    let hook_path = format!("{base}/{id}");
    checked(
        &router,
        "repos/get-webhook",
        "GET",
        &hook_path,
        Some(&outsider),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "repos/get-webhook",
        "GET",
        &hook_path,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        "repos/list-webhooks",
        "GET",
        base,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let config_path = format!("{hook_path}/config");
    checked(
        &router,
        "repos/get-webhook-config-for-repo",
        "GET",
        &config_path,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let (_, config) = checked(
        &router,
        "repos/update-webhook-config-for-repo",
        "PATCH",
        &config_path,
        Some(&writer),
        json!({"secret":"", "insecure_ssl":"0"}),
        StatusCode::OK,
    )
    .await;
    assert!(config.get("secret").is_none());
    let repo = codeyard_db_repos::repositories::records::get_repository(f.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    assert!(codeyard_db_repos::webhooks::get_webhook(
        f.pool(),
        codeyard_db_repos::webhooks::HookOwner::Repository(repo.id),
        id
    )
    .await
    .unwrap()
    .unwrap()
    .encrypted_secret
    .is_none());
    let (_, changed) = checked(
        &router,
        "repos/update-webhook",
        "PATCH",
        &hook_path,
        Some(&writer),
        json!({"add_events":["issues"], "active":false}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(changed["events"], json!(["issues", "push"]));
    assert_eq!(changed["active"], false);
    checked(
        &router,
        "repos/update-webhook",
        "PATCH",
        &hook_path,
        Some(&writer),
        json!({"remove_events":["issues"], "active":true}),
        StatusCode::OK,
    )
    .await;
    // A full update without `config` keeps the stored secret; one with `config` replaces the
    // configuration, so an omitted secret is cleared.
    for (input, path, kept) in [
        (json!({"secret":"kept"}), &config_path, true),
        (json!({"active":true}), &hook_path, true),
        (
            json!({"config":{"url":"https://hooks.example.test/push"}}),
            &hook_path,
            false,
        ),
    ] {
        checked(
            &router,
            if path == &config_path {
                "repos/update-webhook-config-for-repo"
            } else {
                "repos/update-webhook"
            },
            "PATCH",
            path,
            Some(&writer),
            input,
            StatusCode::OK,
        )
        .await;
        let stored = codeyard_db_repos::webhooks::get_webhook(
            f.pool(),
            codeyard_db_repos::webhooks::HookOwner::Repository(repo.id),
            id,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stored.encrypted_secret.is_some(), kept, "{path}");
    }
    checked(
        &router,
        "repos/create-webhook",
        "POST",
        base,
        Some(&writer),
        json!({"config":{"url":"https://hooks.example.test/push"}, "events":["invalid"]}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let record = codeyard_db_core::queries::webhooks::NewDelivery {
        redelivery: false,
        guid: uuid::Uuid::new_v4().to_string(),
        event: "push".into(),
        action: None,
        status_code: Some(200),
        duration_ms: 12,
        error: None,
        request_headers: "Content-Type: application/json\nX-GitHub-Event: push\n".into(),
        request_body: "{\"ref\":\"refs/heads/main\"}".into(),
        response_headers: Some("Content-Type: text/plain\n".into()),
        response_body: Some("ok".into()),
    };
    codeyard_db_repos::webhooks::insert_delivery(f.pool(), id, &record)
        .await
        .unwrap();
    codeyard_db_repos::webhooks::insert_delivery(f.pool(), id, &record)
        .await
        .unwrap();
    let delivery = codeyard_db_repos::webhooks::list_deliveries(f.pool(), id)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.guid == record.guid)
        .unwrap();
    let delivery_path = format!("{hook_path}/deliveries/{}", delivery.id);
    let (_, detail) = checked(
        &router,
        "repos/get-webhook-delivery",
        "GET",
        &delivery_path,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail["request"]["payload"]["ref"], "refs/heads/main");
    let (headers, _) = checked(
        &router,
        "repos/list-webhook-deliveries",
        "GET",
        &format!("{hook_path}/deliveries?per_page=1"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    checked(
        &router,
        "repos/redeliver-webhook-delivery",
        "POST",
        &format!("{delivery_path}/attempts"),
        Some(&writer),
        Value::Null,
        StatusCode::ACCEPTED,
    )
    .await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let rows = codeyard_db_repos::webhooks::list_deliveries(f.pool(), id)
            .await
            .unwrap();
        if let Some(redelivery) = rows.iter().find(|row| row.redelivery) {
            assert_eq!(redelivery.guid, record.guid);
            checked(
                &router,
                "repos/get-webhook-delivery",
                "GET",
                &format!("{hook_path}/deliveries/{}", redelivery.id),
                Some(&reader),
                Value::Null,
                StatusCode::OK,
            )
            .await;
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "redelivery was not recorded"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    for (operation, suffix) in [
        ("repos/ping-webhook", "pings"),
        ("repos/test-push-webhook", "tests"),
    ] {
        checked(
            &router,
            operation,
            "POST",
            &format!("{hook_path}/{suffix}"),
            Some(&writer),
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    }
    checked(
        &router,
        "repos/delete-webhook",
        "DELETE",
        &hook_path,
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &router,
        "repos/get-webhook",
        "GET",
        &hook_path,
        Some(&writer),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
}

#[tokio::test]
async fn sqlite_github_webhooks() {
    hooks_contract(false).await;
}
#[tokio::test]
async fn postgres_github_webhooks() {
    hooks_contract(true).await;
}

async fn deploy_keys_contract(pg: bool) {
    const PUBLIC_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let reader = credential(&router, &f, alice, "read").await;
    let outsider = credential(&router, &f, bob, "write").await;
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"proj", "private":true}),
        StatusCode::CREATED,
    )
    .await;
    let base = "/api/v3/repos/alice/proj/keys";
    checked(
        &router,
        "repos/create-deploy-key",
        "POST",
        base,
        Some(&reader),
        json!({"key":PUBLIC_KEY}),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/create-deploy-key",
        "POST",
        base,
        Some(&writer),
        json!({"key":"invalid", "title":"Invalid"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let (_, key) = checked(
        &router,
        "repos/create-deploy-key",
        "POST",
        base,
        Some(&writer),
        json!({"key":PUBLIC_KEY, "title":"CI", "read_only":false}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(key["key"], PUBLIC_KEY);
    assert_eq!(key["added_by"], "alice");
    assert_eq!(key["read_only"], false);
    let path = format!("{base}/{}", key["id"]);
    checked(
        &router,
        "repos/get-deploy-key",
        "GET",
        &path,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        "repos/get-deploy-key",
        "GET",
        &path,
        Some(&outsider),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let (_, keys) = checked(
        &router,
        "repos/list-deploy-keys",
        "GET",
        base,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(keys.as_array().unwrap().len(), 1);
    checked(
        &router,
        "repos/create-deploy-key",
        "POST",
        base,
        Some(&writer),
        json!({"key":PUBLIC_KEY, "title":"Duplicate"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &router,
        "repos/delete-deploy-key",
        "DELETE",
        &path,
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &router,
        "repos/get-deploy-key",
        "GET",
        &path,
        Some(&reader),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let (_, key) = checked(
        &router,
        "repos/create-deploy-key",
        "POST",
        base,
        Some(&writer),
        json!({"key":PUBLIC_KEY}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(key["read_only"], true);
    let repo = codeyard_db_repos::repositories::records::get_repository(f.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let actor = codeyard_db_identity::users::accounts::get_user_by_id(f.pool(), alice)
        .await
        .unwrap()
        .unwrap();
    let token_id = f
        .scalar(
            "SELECT id FROM personal_access_tokens WHERE user_id=$1 AND name='write'",
            &[alice],
        )
        .await;
    let read_id = f
        .scalar(
            "SELECT id FROM personal_access_tokens WHERE user_id=$1 AND name='read'",
            &[alice],
        )
        .await;
    let id = key["id"].as_i64().unwrap();
    use codeyard_repos::deploy_keys::remove_key;

    assert!(matches!(
        remove_key(
            f.pool(),
            &actor,
            codeyard_db_core::queries::users::accounts::OwnedAccountCredential::Token(
                read_id,
                codeyard_support::token_hash::token_hash(reader.strip_prefix("Bearer ").unwrap())
            ),
            &repo,
            id
        )
        .await,
        Err(codeyard_model::repositories::DeployKeyError::Denied)
    ));
    f.execute(
        "UPDATE personal_access_tokens SET expires_at='2000-01-01T00:00:00Z' WHERE id=$1",
        &[token_id],
    )
    .await;
    assert!(matches!(
        remove_key(
            f.pool(),
            &actor,
            codeyard_db_core::queries::users::accounts::OwnedAccountCredential::Token(
                token_id,
                codeyard_support::token_hash::token_hash(writer.strip_prefix("Bearer ").unwrap())
            ),
            &repo,
            id
        )
        .await,
        Err(codeyard_model::repositories::DeployKeyError::Denied)
    ));
    checked(
        &router,
        "repos/get-deploy-key",
        "GET",
        &format!("{base}/{id}"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
}

#[tokio::test]
async fn sqlite_github_deploy_keys() {
    deploy_keys_contract(false).await;
}
#[tokio::test]
async fn postgres_github_deploy_keys() {
    deploy_keys_contract(true).await;
}

async fn archives_contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let reader = credential(&router, &f, alice, "read").await;
    let outsider = credential(&router, &f, bob, "read").await;
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"proj", "private":true, "auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    for (segment, operation, mime) in [
        (
            "zipball",
            "repos/download-zipball-archive",
            "application/zip",
        ),
        (
            "tarball",
            "repos/download-tarball-archive",
            "application/gzip",
        ),
    ] {
        let url = format!("/api/v3/repos/alice/proj/{segment}");
        checked(
            &router,
            operation,
            "GET",
            &format!("{url}/main"),
            Some(&outsider),
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
        checked(
            &router,
            operation,
            "GET",
            &format!("{url}/missing"),
            Some(&reader),
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
        let get = |url: &str| {
            router.clone().oneshot(
                Request::builder()
                    .uri(url)
                    .header("authorization", &reader)
                    .body(Body::empty())
                    .unwrap(),
            )
        };
        for reference in [url.clone(), format!("{url}/main")] {
            let response = get(&reference).await.unwrap();
            assert_eq!(response.status(), StatusCode::FOUND);
            let location = response.headers()["location"].to_str().unwrap().to_owned();
            assert!(location.starts_with("http://localhost:3000/api/v3/repos/alice/proj/"));
            assert!(to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .is_empty());
            let path = location
                .parse::<axum::http::Uri>()
                .unwrap()
                .path_and_query()
                .unwrap()
                .to_string();
            let response = get(&path).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["content-type"], mime);
            assert!(response.headers()["content-disposition"]
                .to_str()
                .unwrap()
                .contains("alice-proj-"));
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            if segment == "zipball" {
                assert!(bytes.starts_with(b"PK\x03\x04"));
            } else {
                let mut tar = Vec::new();
                std::io::Read::read_to_end(
                    &mut flate2::read::GzDecoder::new(bytes.as_ref()),
                    &mut tar,
                )
                .unwrap();
                assert!(tar
                    .windows(b"README.md".len())
                    .any(|part| part == b"README.md"));
                assert!(tar.windows(b"# proj".len()).any(|part| part == b"# proj"));
            }
        }
    }
}

#[tokio::test]
async fn sqlite_github_archives() {
    archives_contract(false).await;
}
#[tokio::test]
async fn postgres_github_archives() {
    archives_contract(true).await;
}

async fn statistics_contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let reader = credential(&router, &f, alice, "read").await;
    let outsider = credential(&router, &f, bob, "read").await;
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"proj", "private":true, "auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let base = "/api/v3/repos/alice/proj";
    for (operation, suffix) in [
        ("repos/get-code-frequency-stats", "code_frequency"),
        ("repos/get-commit-activity-stats", "commit_activity"),
        ("repos/get-contributors-stats", "contributors"),
        ("repos/get-participation-stats", "participation"),
        ("repos/get-punch-card-stats", "punch_card"),
    ] {
        let path = format!("{base}/stats/{suffix}");
        checked(
            &router,
            operation,
            "GET",
            &path,
            Some(&outsider),
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
        let (_, value) = checked(
            &router,
            operation,
            "GET",
            &path,
            Some(&reader),
            Value::Null,
            StatusCode::OK,
        )
        .await;
        match suffix {
            "code_frequency" => {
                assert!(value
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|week| week[1].as_i64().unwrap() > 0));
                assert!(value
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|week| week[2].as_i64().unwrap() <= 0));
            }
            "commit_activity" => {
                assert_eq!(value.as_array().unwrap().len(), 52);
                assert_eq!(
                    value
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|week| week["total"].as_u64().unwrap())
                        .sum::<u64>(),
                    1
                );
            }
            "contributors" => {
                assert_eq!(value[0]["author"]["login"], "alice");
                assert_eq!(value[0]["total"], 1);
            }
            "participation" => {
                for key in ["all", "owner"] {
                    assert_eq!(value[key].as_array().unwrap().len(), 52);
                    assert_eq!(
                        value[key]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|n| n.as_u64().unwrap())
                            .sum::<u64>(),
                        1
                    );
                }
            }
            "punch_card" => {
                assert_eq!(value.as_array().unwrap().len(), 168);
                assert_eq!(
                    value
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|hour| hour[2].as_u64().unwrap())
                        .sum::<u64>(),
                    1
                );
            }
            _ => unreachable!(),
        }
    }
    let (_, initial) = checked(
        &router,
        "repos/list-contributors",
        "GET",
        &format!("{base}/contributors"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(initial[0]["login"], "alice");
    let repo = codeyard_db_repos::repositories::records::get_repository(f.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        let parent = git
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let mut tree = git.treebuilder(Some(&parent.tree().unwrap())).unwrap();
        tree.insert("added.txt", git.blob(b"one\ntwo\n").unwrap(), 0o100644)
            .unwrap();
        let tree = git.find_tree(tree.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Anonymous", "anonymous@example.test").unwrap();
        git.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Add lines",
            &tree,
            &[&parent],
        )
        .unwrap();
    }
    let (_, contributors) = checked(
        &router,
        "repos/list-contributors",
        "GET",
        &format!("{base}/contributors?anon=true"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(contributors.as_array().unwrap().len(), 2);
    assert!(contributors
        .as_array()
        .unwrap()
        .iter()
        .any(|author| author["type"] == "Anonymous" && author["contributions"] == 1));
    let (headers, _) = checked(
        &router,
        "repos/list-contributors",
        "GET",
        &format!("{base}/contributors?anon=1&per_page=1"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (_, value) = checked(
        &router,
        "repos/get-participation-stats",
        "GET",
        &format!("{base}/stats/participation"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        value["all"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_u64().unwrap())
            .sum::<u64>(),
        2
    );
    assert_eq!(
        value["owner"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_u64().unwrap())
            .sum::<u64>(),
        1
    );
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"empty"}),
        StatusCode::CREATED,
    )
    .await;
    checked(
        &router,
        "repos/list-contributors",
        "GET",
        "/api/v3/repos/alice/empty/contributors",
        Some(&reader),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &router,
        "repos/get-code-frequency-stats",
        "GET",
        "/api/v3/repos/alice/empty/stats/code_frequency",
        Some(&reader),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
}

#[tokio::test]
async fn sqlite_github_statistics() {
    statistics_contract(false).await;
}
#[tokio::test]
async fn postgres_github_statistics() {
    statistics_contract(true).await;
}

async fn collaborators_contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let carol = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let reader = credential(&router, &f, alice, "read").await;
    let recipient = credential(&router, &f, bob, "write").await;
    let recipient_read = credential(&router, &f, bob, "read").await;
    let outsider = credential(&router, &f, carol, "write").await;
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"proj","private":true}),
        StatusCode::CREATED,
    )
    .await;
    let base = "/api/v3/repos/alice/proj";
    let target = format!("{base}/collaborators/bob");
    checked(
        &router,
        "repos/add-collaborator",
        "PUT",
        &target,
        Some(&reader),
        json!({}),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/add-collaborator",
        "PUT",
        &target,
        Some(&outsider),
        json!({}),
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "repos/add-collaborator",
        "PUT",
        &format!("{base}/collaborators/alice"),
        Some(&writer),
        json!({}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &router,
        "repos/add-collaborator",
        "PUT",
        &target,
        Some(&writer),
        json!({"permission":"owner"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let (_, invitation) = checked(
        &router,
        "repos/add-collaborator",
        "PUT",
        &target,
        Some(&writer),
        json!({"permission":"push"}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(invitation["permissions"], "write");
    assert_eq!(invitation["invitee"]["login"], "bob");
    let id = invitation["id"].as_i64().unwrap();
    let response_path = format!("/api/v3/user/repository_invitations/{id}");
    let invitation_path = format!("{base}/invitations/{id}");
    checked(
        &router,
        "repos/get",
        "GET",
        base,
        Some(&recipient),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "repos/check-collaborator",
        "GET",
        &target,
        Some(&reader),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let (_, own) = checked(
        &router,
        "repos/list-invitations-for-authenticated-user",
        "GET",
        "/api/v3/user/repository_invitations?per_page=1",
        Some(&recipient_read),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(own[0]["id"], id);
    let (_, empty) = checked(
        &router,
        "repos/list-invitations-for-authenticated-user",
        "GET",
        "/api/v3/user/repository_invitations",
        Some(&outsider),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(empty.as_array().unwrap().is_empty());
    let (_, pending) = checked(
        &router,
        "repos/list-invitations",
        "GET",
        &format!("{base}/invitations"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(pending[0]["id"], id);
    checked(
        &router,
        "repos/accept-invitation-for-authenticated-user",
        "PATCH",
        &response_path,
        Some(&writer),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "repos/accept-invitation-for-authenticated-user",
        "PATCH",
        &response_path,
        Some(&recipient_read),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    let (_, changed) = checked(
        &router,
        "repos/update-invitation",
        "PATCH",
        &invitation_path,
        Some(&writer),
        json!({"permissions":"triage"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(changed["permissions"], "triage");
    checked(
        &router,
        "repos/accept-invitation-for-authenticated-user",
        "PATCH",
        &response_path,
        Some(&recipient),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &router,
        "repos/get",
        "GET",
        base,
        Some(&recipient_read),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        "repos/check-collaborator",
        "GET",
        &target,
        Some(&reader),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let (_, level) = checked(
        &router,
        "repos/get-collaborator-permission-level",
        "GET",
        &format!("{target}/permission"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(level["permission"], "read");
    assert_eq!(level["role_name"], "triage");
    assert_eq!(level["user"]["permissions"]["triage"], true);
    assert_eq!(level["user"]["permissions"]["push"], false);
    let (_, people) = checked(
        &router,
        "repos/list-collaborators",
        "GET",
        &format!("{base}/collaborators?affiliation=outside&permission=triage"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(people.as_array().unwrap().len(), 1);
    assert_eq!(people[0]["login"], "bob");
    let (headers, _) = checked(
        &router,
        "repos/list-collaborators",
        "GET",
        &format!("{base}/collaborators?per_page=1"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    checked(
        &router,
        "repos/add-collaborator",
        "PUT",
        &target,
        Some(&writer),
        json!({"permission":"maintain"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &router,
        "repos/remove-collaborator",
        "DELETE",
        &target,
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &router,
        "repos/get",
        "GET",
        base,
        Some(&recipient_read),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let (_, invite) = checked(
        &router,
        "repos/add-collaborator",
        "PUT",
        &target,
        Some(&writer),
        json!({}),
        StatusCode::CREATED,
    )
    .await;
    checked(
        &router,
        "repos/decline-invitation-for-authenticated-user",
        "DELETE",
        &format!("/api/v3/user/repository_invitations/{}", invite["id"]),
        Some(&recipient),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let (_, invite) = checked(
        &router,
        "repos/add-collaborator",
        "PUT",
        &target,
        Some(&writer),
        json!({}),
        StatusCode::CREATED,
    )
    .await;
    checked(
        &router,
        "repos/delete-invitation",
        "DELETE",
        &format!("{base}/invitations/{}", invite["id"]),
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &router,
        "repos/accept-invitation-for-authenticated-user",
        "PATCH",
        &format!("/api/v3/user/repository_invitations/{}", invite["id"]),
        Some(&recipient),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let (_, invite) = checked(
        &router,
        "repos/add-collaborator",
        "PUT",
        &target,
        Some(&writer),
        json!({}),
        StatusCode::CREATED,
    )
    .await;
    let expired_id = invite["id"].as_i64().unwrap();
    f.execute(
        "UPDATE repository_invitations SET created_at='2000-01-01T00:00:00Z' WHERE id=$1",
        &[expired_id],
    )
    .await;
    checked(
        &router,
        "repos/accept-invitation-for-authenticated-user",
        "PATCH",
        &format!("/api/v3/user/repository_invitations/{expired_id}"),
        Some(&recipient),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let owner = Client::login(&router, f.pool(), alice).await;
    let repo = codeyard_db_repos::repositories::records::get_repository(f.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let native = owner
        .form_reply(
            "/alice/proj/_forms/invite-collaborator",
            &[
                ("repository_id", &repo.id.to_string()),
                ("username", "bob"),
                ("access", "write"),
            ],
        )
        .await;
    assert_eq!(native.status, StatusCode::SEE_OTHER, "{}", native.text());
    let html = owner
        .page_ok("/alice/proj/settings?tab=collaborators")
        .await;
    assert!(
        html.contains("Pending invitations") && html.contains("bob"),
        "{html}"
    );
    let pending = codeyard_db_repos::repositories::collaborators::invitations(
        f.pool(),
        alice,
        Some(repo.id),
        0,
        100,
    )
    .await
    .unwrap();
    let cancelled = owner
        .form_reply(
            "/alice/proj/_forms/cancel-invitation",
            &[
                ("repository_id", &repo.id.to_string()),
                ("invitation_id", &pending[0].id.to_string()),
            ],
        )
        .await;
    assert_eq!(
        cancelled.status,
        StatusCode::SEE_OTHER,
        "{}",
        cancelled.text()
    );
    assert!(codeyard_db_repos::repositories::collaborators::invitations(
        f.pool(),
        alice,
        Some(repo.id),
        0,
        100
    )
    .await
    .unwrap()
    .is_empty());

    use codeyard_db_core::queries::organizations::OrganizationOutcome;
    use codeyard_db_orgs::organizations::membership;
    use codeyard_db_orgs::organizations::profiles;
    let input = profiles::NewOrganization::new(
        "acme",
        profiles::OrganizationProfile::new(None, None).unwrap(),
    )
    .unwrap();
    let OrganizationOutcome::Applied(org) = profiles::create_organization(f.pool(), alice, &input)
        .await
        .unwrap()
    else {
        panic!("organization not created")
    };
    assert!(matches!(
        membership::add_organization_member(
            f.pool(),
            alice,
            org.id,
            bob,
            codeyard_db_core::models::OrgRole::Member
        )
        .await
        .unwrap(),
        OrganizationOutcome::Applied(_)
    ));
    let input = codeyard_db_orgs::organizations::teams::NewTeam::new(
        "core",
        codeyard_db_orgs::organizations::teams::TeamProfile::new(
            "Core",
            None,
            codeyard_db_orgs::organizations::teams::TeamPrivacy::Visible,
        )
        .unwrap(),
    )
    .unwrap();
    let OrganizationOutcome::Applied(team) =
        codeyard_db_orgs::organizations::teams::create_organization_team(
            f.pool(),
            alice,
            org.id,
            &input,
        )
        .await
        .unwrap()
    else {
        panic!("team not created")
    };
    assert!(matches!(
        codeyard_db_orgs::organizations::teams::add_organization_team_member(
            f.pool(),
            alice,
            org.id,
            team.id,
            bob,
            codeyard_db_orgs::organizations::teams::TeamRole::Member
        )
        .await
        .unwrap(),
        OrganizationOutcome::Applied(_)
    ));
    let (_, org_repo) = checked(
        &router,
        "repos/create-in-org",
        "POST",
        "/api/v3/orgs/acme/repos",
        Some(&writer),
        json!({"name":"shared","private":true}),
        StatusCode::CREATED,
    )
    .await;
    assert!(matches!(
        codeyard_db_orgs::organizations::teams::grant_organization_team_repository(
            f.pool(),
            alice,
            org.id,
            team.id,
            org_repo["id"].as_i64().unwrap(),
            codeyard_db_orgs::organizations::teams::TeamRepositoryPermission::new(4).unwrap()
        )
        .await
        .unwrap(),
        OrganizationOutcome::Applied(_)
    ));
    let (_, people) = checked(
        &router,
        "repos/list-collaborators",
        "GET",
        "/api/v3/repos/acme/shared/collaborators",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(people
        .as_array()
        .unwrap()
        .iter()
        .any(|person| person["login"] == "bob" && person["role_name"] == "maintain"));
    let (_, people) = checked(
        &router,
        "repos/list-collaborators",
        "GET",
        "/api/v3/repos/acme/shared/collaborators?affiliation=outside",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(people.as_array().unwrap().is_empty());
    checked(
        &router,
        "repos/add-collaborator",
        "PUT",
        "/api/v3/repos/acme/shared/collaborators/bob",
        Some(&writer),
        json!({"permission":"pull"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &router,
        "repos/remove-collaborator",
        "DELETE",
        "/api/v3/repos/acme/shared/collaborators/bob",
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let (_, level) = checked(
        &router,
        "repos/get-collaborator-permission-level",
        "GET",
        "/api/v3/repos/acme/shared/collaborators/bob/permission",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(level["role_name"], "maintain");
}

#[tokio::test]
async fn sqlite_github_collaborators() {
    collaborators_contract(false).await;
}
#[tokio::test]
async fn postgres_github_collaborators() {
    collaborators_contract(true).await;
}

async fn update_contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let reader = credential(&router, &f, alice, "read").await;
    let outsider = credential(&router, &f, bob, "write").await;
    let (_, created) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"proj","auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let base = "/api/v3/repos/alice/proj";
    checked(
        &router,
        "repos/update",
        "PATCH",
        base,
        Some(&reader),
        json!({"description":"Denied"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/update",
        "PATCH",
        base,
        Some(&outsider),
        json!({"description":"Denied"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    let (_,updated)=checked(&router,"repos/update","PATCH",base,Some(&writer),json!({"name":"renamed","description":"Updated","homepage":"https://example.test","private":true,"allow_merge_commit":false,"allow_squash_merge":true,"allow_rebase_merge":false}),StatusCode::OK).await;
    assert_eq!(updated["name"], "renamed");
    assert_eq!(updated["description"], "Updated");
    assert_eq!(updated["homepage"], "https://example.test/");
    assert_eq!(updated["private"], true);
    assert_eq!(updated["allow_merge_commit"], false);
    let base = "/api/v3/repos/alice/renamed";
    for payload in [
        json!({"description":"Must not persist","allow_squash_merge":false}),
        json!({"description":"Must not persist","default_branch":"missing"}),
        json!({"description":"Must not persist","homepage":"javascript:alert(1)"}),
    ] {
        checked(
            &router,
            "repos/update",
            "PATCH",
            base,
            Some(&writer),
            payload,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let (_, current) = checked(
        &router,
        "repos/get",
        "GET",
        base,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(current["description"], "Updated");
    assert_eq!(current["homepage"], "https://example.test/");
    let repo = f.repository(created["id"].as_i64().unwrap()).await;
    let refs = codeyard_app::repositories::store::repo_store(&repo, None)
        .refs()
        .await
        .unwrap();
    {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        git.reference(
            "refs/heads/dev",
            git2::Oid::from_str(&refs["refs/heads/main"]).unwrap(),
            false,
            "Fixture branch",
        )
        .unwrap();
    }
    let (_, archived) = checked(
        &router,
        "repos/update",
        "PATCH",
        base,
        Some(&writer),
        json!({"default_branch":"dev","archived":true}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(archived["default_branch"], "dev");
    assert_eq!(archived["archived"], true);
    assert_eq!(
        git2::Repository::open_bare(&repo.disk_path)
            .unwrap()
            .head()
            .unwrap()
            .name(),
        Some("refs/heads/dev")
    );
    checked(
        &router,
        "repos/update",
        "PATCH",
        base,
        Some(&writer),
        json!({"description":"Archived"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    let (_, active) = checked(
        &router,
        "repos/update",
        "PATCH",
        base,
        Some(&writer),
        json!({"archived":false,"description":null,"homepage":null,"visibility":"public"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(active["archived"], false);
    assert!(active["description"].is_null());
    assert!(active["homepage"].is_null());
    assert_eq!(active["private"], false);
    checked(
        &router,
        "repos/get",
        "GET",
        base,
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let (_, wiki) = checked(
        &router,
        "repos/update",
        "PATCH",
        base,
        Some(&writer),
        json!({"has_wiki":true}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(wiki["has_wiki"], true);
    // The settings `gh repo edit` sends are stored and read back, as on GitHub.
    let settings = json!({"has_issues":false,"has_projects":false,"allow_forking":false,
        "delete_branch_on_merge":true,"allow_update_branch":true,"allow_auto_merge":true,
        "squash_merge_commit_title":"PR_TITLE","squash_merge_commit_message":"PR_BODY"});
    checked(
        &router,
        "repos/update",
        "PATCH",
        base,
        Some(&writer),
        settings.clone(),
        StatusCode::OK,
    )
    .await;
    let (_, read) = checked(
        &router,
        "repos/get",
        "GET",
        base,
        Some(&writer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    for (key, value) in settings.as_object().unwrap() {
        assert_eq!(&read[key], value, "{key}");
    }
    // GitHub pairs COMMIT_OR_PR_TITLE only with COMMIT_MESSAGES.
    let mismatched = json!({"squash_merge_commit_title":"COMMIT_OR_PR_TITLE","squash_merge_commit_message":"BLANK"});
    checked(
        &router,
        "repos/update",
        "PATCH",
        base,
        Some(&writer),
        mismatched,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    // Only an organization's repository can be internal.
    let internal = json!({"name":"inside","visibility":"internal"});
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        internal,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    // `gh repo delete` sends this request after its confirmation prompt.
    checked(
        &router,
        "repos/delete",
        "DELETE",
        base,
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &router,
        "repos/get",
        "GET",
        base,
        Some(&writer),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
}
#[tokio::test]
async fn sqlite_github_repository_updates() {
    update_contract(false).await;
}
#[tokio::test]
async fn postgres_github_repository_updates() {
    update_contract(true).await;
}

async fn metadata_contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for (actor, scope) in [(alice, "write"), (alice, "read"), (bob, "write")] {
        let client = Client::login(&router, f.pool(), actor).await;
        let token = tokens::create(
            f.pool(),
            actor,
            Some(&client.session),
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
    let (writer, reader, outsider) = (&credentials[0], &credentials[1], &credentials[2]);
    let (_, repo) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(writer),
        json!({"name":"private", "private":true, "auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let private = "/api/v3/repos/alice/private";
    assert_eq!(repo["private"], true);
    let stored = f.repository(repo["id"].as_i64().unwrap()).await;
    tokio::task::spawn_blocking(move || {
        let git = git2::Repository::open_bare(stored.disk_path).unwrap();
        let commit = git.head().unwrap().peel_to_commit().unwrap();
        git.tag_lightweight("v1", commit.as_object(), false)
            .unwrap();
    })
    .await
    .unwrap();
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(writer),
        json!({"name":"private"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(writer),
        json!({"name":"../bad"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    for (operation, suffix) in [
        ("repos/get", ""),
        ("repos/get-readme", "/readme"),
        ("repos/get-all-topics", "/topics"),
        ("repos/list-languages", "/languages"),
        ("repos/list-tags", "/tags"),
        ("repos/get-hash-algorithm", "/hash-algorithm"),
    ] {
        checked(
            &router,
            operation,
            "GET",
            &format!("{private}{suffix}"),
            Some(reader),
            Value::Null,
            StatusCode::OK,
        )
        .await;
        if !matches!(operation, "repos/list-languages" | "repos/list-tags") {
            checked(
                &router,
                operation,
                "GET",
                &format!("{private}{suffix}"),
                Some(outsider),
                Value::Null,
                StatusCode::NOT_FOUND,
            )
            .await;
        }
    }
    checked(
        &router,
        "repos/replace-all-topics",
        "PUT",
        &format!("{private}/topics"),
        Some(writer),
        json!({"names":["bad topic"]}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let (_, topics) = checked(
        &router,
        "repos/replace-all-topics",
        "PUT",
        &format!("{private}/topics"),
        Some(writer),
        json!({"names":["rust", "web"]}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(topics["names"], json!(["rust", "web"]));
    checked(
        &router,
        "repos/create-or-update-file-contents",
        "PUT",
        &format!("{private}/contents/docs/README.md"),
        Some(writer),
        json!({"message":"Add docs", "content":"IyBEb2NzCg=="}),
        StatusCode::CREATED,
    )
    .await;
    checked(
        &router,
        "repos/get-readme-in-directory",
        "GET",
        &format!("{private}/readme/docs"),
        Some(reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        "repos/get-readme-in-directory",
        "GET",
        &format!("{private}/readme/missing"),
        Some(reader),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let (_, public) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(writer),
        json!({"name":"public", "auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    for (operation, path, token, expected_count) in [
        (
            "repos/list-for-authenticated-user",
            "/api/v3/user/repos",
            Some(reader.as_str()),
            2,
        ),
        ("repos/list-for-user", "/api/v3/users/alice/repos", None, 1),
        ("repos/list-public", "/api/v3/repositories", None, 1),
        (
            "repos/list-public",
            "/api/v3/repositories?since=9999999",
            None,
            0,
        ),
        (
            "repos/list-forks",
            "/api/v3/repos/alice/public/forks",
            None,
            0,
        ),
    ] {
        let (_, body) = checked(
            &router,
            operation,
            "GET",
            path,
            token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), expected_count, "{body}");
    }
    let (headers, page) = checked(
        &router,
        "repos/list-for-authenticated-user",
        "GET",
        "/api/v3/user/repos?per_page=1",
        Some(reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let input = codeyard_db_orgs::organizations::profiles::NewOrganization::new(
        "acme",
        codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(Some("Acme"), None)
            .unwrap(),
    )
    .unwrap();
    let org =
        codeyard_db_orgs::organizations::profiles::create_organization(f.pool(), alice, &input)
            .await
            .unwrap();
    assert!(matches!(
        org,
        codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(_)
    ));
    checked(
        &router,
        "repos/create-in-org",
        "POST",
        "/api/v3/orgs/acme/repos",
        Some(writer),
        json!({"name":"org-private", "private":true}),
        StatusCode::CREATED,
    )
    .await;
    checked(
        &router,
        "repos/create-in-org",
        "POST",
        "/api/v3/orgs/acme/repos",
        Some(outsider),
        json!({"name":"denied"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    for (token, expected) in [
        (Some(reader.as_str()), 1),
        (Some(outsider.as_str()), 0),
        (None, 0),
    ] {
        let (_, body) = checked(
            &router,
            "repos/list-for-org",
            "GET",
            "/api/v3/orgs/acme/repos",
            token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), expected);
    }
    checked(
        &router,
        "repos/transfer",
        "POST",
        "/api/v3/repos/alice/public/transfer",
        Some(writer),
        json!({"new_owner":"acme"}),
        StatusCode::ACCEPTED,
    )
    .await;
    checked(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/public",
        Some(reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        "repos/delete",
        "DELETE",
        "/api/v3/repos/acme/public",
        Some(writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert!(
        codeyard_db_repos::repositories::records::get_repository_by_id(
            f.pool(),
            public["id"].as_i64().unwrap()
        )
        .await
        .unwrap()
        .is_none()
    );
    checked(
        &router,
        "repos/delete",
        "DELETE",
        private,
        Some(reader),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/delete",
        "DELETE",
        private,
        Some(writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
}

async fn releases_contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for actor in [alice, bob] {
        let client = Client::login(&router, f.pool(), actor).await;
        let token = tokens::create(
            f.pool(),
            actor,
            Some(&client.session),
            TokenInput {
                name: "releases",
                scope: "write",
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let (writer, outsider) = (&credentials[0], &credentials[1]);
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(writer),
        json!({"name":"public", "auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let base = "/api/v3/repos/alice/public/releases";
    checked(
        &router,
        "repos/get-latest-release",
        "GET",
        &format!("{base}/latest"),
        None,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "repos/create-release",
        "POST",
        base,
        Some(outsider),
        json!({"tag_name":"denied"}),
        StatusCode::NOT_FOUND,
    )
    .await;
    let (_, release) = checked(
        &router,
        "repos/create-release",
        "POST",
        base,
        Some(writer),
        json!({"tag_name":"v1.0", "name":"First release", "body":"Release notes"}),
        StatusCode::CREATED,
    )
    .await;
    let id = release["id"].as_i64().unwrap();
    assert_eq!(release["author"]["login"], "alice");
    for payload in [json!({"tag_name":"v1.0"}), json!({"tag_name":"bad..tag"})] {
        checked(
            &router,
            "repos/create-release",
            "POST",
            base,
            Some(writer),
            payload,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let (_, draft) = checked(
        &router,
        "repos/create-release",
        "POST",
        base,
        Some(writer),
        json!({"tag_name":"v2.0", "draft":true}),
        StatusCode::CREATED,
    )
    .await;
    let reader = credential(&router, &f, alice, "read").await;
    checked(
        &router,
        "repos/update-release",
        "PATCH",
        &format!("{base}/{id}"),
        Some(&reader),
        json!({"name":"Denied"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    let (_, edited) = checked(
        &router,
        "repos/update-release",
        "PATCH",
        &format!("{base}/{id}"),
        Some(writer),
        json!({"name":"Edited release", "body":null}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(edited["name"], "Edited release");
    assert!(edited["body"].is_null());
    checked(
        &router,
        "repos/update-release",
        "PATCH",
        &format!("{base}/{id}"),
        Some(writer),
        json!({"make_latest":"invalid"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "{base}/{id}/assets?name=artifact.txt&label=Artifact"
                ))
                .header("authorization", writer)
                .header("content-type", "text/plain")
                .body(Body::from("artifact\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let asset: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_github_schema(
        "repos/upload-release-asset",
        StatusCode::CREATED.as_u16(),
        &asset,
    );
    assert_eq!(asset["content_type"], "text/plain");
    let asset_path = format!("{base}/assets/{}", asset["id"]);
    checked(
        &router,
        "repos/update-release-asset",
        "PATCH",
        &asset_path,
        Some(&reader),
        json!({"name":"denied.txt"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    let (_, renamed) = checked(
        &router,
        "repos/update-release-asset",
        "PATCH",
        &asset_path,
        Some(writer),
        json!({"name":"renamed.txt", "label":null}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(renamed["name"], "renamed.txt");
    assert!(renamed["label"].is_null());
    checked(
        &router,
        "repos/update-release-asset",
        "PATCH",
        &asset_path,
        Some(writer),
        json!({"name":"../invalid"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{base}/{id}/assets?name=large.bin"))
                .header("authorization", writer)
                .header("content-type", "application/octet-stream")
                .body(Body::from(vec![b'b'; 4 * 1024 * 1024 + 1]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let large: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_github_schema(
        "repos/upload-release-asset",
        StatusCode::CREATED.as_u16(),
        &large,
    );
    assert_eq!(large["size"], 4 * 1024 * 1024 + 1);
    // A token revoked while its upload body is arriving publishes nothing.
    let client = Client::login(&router, f.pool(), alice).await;
    let doomed = tokens::create(
        f.pool(),
        alice,
        Some(&client.session),
        TokenInput {
            name: "doomed",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let (pool, session, token_id) = (f.pool().clone(), client.session.clone(), doomed.token.id);
    let body = Body::from_stream(futures_util::stream::once(async move {
        assert!(codeyard_db_identity::users::tokens::delete_token(
            &pool, alice, &session, token_id
        )
        .await
        .unwrap());
        Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"late\n"))
    }));
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{base}/{id}/assets?name=late.txt"))
                .header("authorization", format!("Bearer {}", doomed.value))
                .header("content-type", "text/plain")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    checked(
        &router,
        "repos/update-release-asset",
        "PATCH",
        &asset_path,
        Some(writer),
        json!({"name":"large.bin"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let (_, asset_json) = checked(
        &router,
        "repos/get-release-asset",
        "GET",
        &asset_path,
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(asset_json["uploader"]["id"], alice);
    assert_eq!(asset_json["state"], "uploaded");
    assert!(asset_json["digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    let downloaded = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&asset_path)
                .header("accept", "application/octet-stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(downloaded.status(), StatusCode::OK);
    assert!(downloaded.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .contains("renamed.txt"));
    assert_eq!(
        to_bytes(downloaded.into_body(), 1024)
            .await
            .unwrap()
            .as_ref(),
        b"artifact\n"
    );
    let (_, asset_json) = checked(
        &router,
        "repos/get-release-asset",
        "GET",
        &asset_path,
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(asset_json["download_count"], 1);
    for (operation, path, token) in [
        ("repos/get-release", format!("{base}/{id}"), None),
        (
            "repos/get-release-by-tag",
            format!("{base}/tags/v1.0"),
            None,
        ),
        ("repos/get-latest-release", format!("{base}/latest"), None),
        (
            "repos/get-release-by-tag",
            format!("{base}/tags/v2.0"),
            Some(writer.as_str()),
        ),
        (
            "repos/list-release-assets",
            format!("{base}/{id}/assets"),
            None,
        ),
    ] {
        checked(
            &router,
            operation,
            "GET",
            &path,
            token,
            Value::Null,
            StatusCode::OK,
        )
        .await;
    }
    checked(
        &router,
        "repos/get-release-by-tag",
        "GET",
        &format!("{base}/tags/v2.0"),
        None,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let (_, visible) = checked(
        &router,
        "repos/list-releases",
        "GET",
        base,
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(visible.as_array().unwrap().len(), 1);
    let (headers, page) = checked(
        &router,
        "repos/list-releases",
        "GET",
        &format!("{base}?per_page=1"),
        Some(writer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    checked(
        &router,
        "repos/delete-release-asset",
        "DELETE",
        &asset_path,
        Some(writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &router,
        "repos/get-release-asset",
        "GET",
        &asset_path,
        None,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    for release in [draft["id"].as_i64().unwrap(), id] {
        checked(
            &router,
            "repos/delete-release",
            "DELETE",
            &format!("{base}/{release}"),
            Some(writer),
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    }
    checked(
        &router,
        "repos/get-latest-release",
        "GET",
        &format!("{base}/latest"),
        None,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    // discussion_category_name: an unknown category is refused before anything is saved, and
    // publishing a draft starts the discussion its category asked for.
    let category = json!({"tag_name":"v3.0","discussion_category_name":"General"});
    checked(
        &router,
        "repos/create-release",
        "POST",
        base,
        Some(writer),
        category.clone(),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/alice/public",
        Some(writer),
        json!({"has_discussions":true}),
        StatusCode::OK,
    )
    .await;
    let missing = json!({"tag_name":"v3.0","discussion_category_name":"Missing"});
    checked(
        &router,
        "repos/create-release",
        "POST",
        base,
        Some(writer),
        missing,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let (_, started) = checked(
        &router,
        "repos/create-release",
        "POST",
        base,
        Some(writer),
        category,
        StatusCode::CREATED,
    )
    .await;
    assert!(
        started["discussion_url"]
            .as_str()
            .is_some_and(|url| url.contains("/alice/public/discussions/")),
        "{started}"
    );
    let (_, draft) = checked(
        &router,
        "repos/create-release",
        "POST",
        base,
        Some(writer),
        json!({"tag_name":"v3.1","draft":true,"discussion_category_name":"ideas"}),
        StatusCode::CREATED,
    )
    .await;
    assert!(draft["discussion_url"].is_null(), "{draft}");
    let (_, published) = checked(
        &router,
        "repos/update-release",
        "PATCH",
        &format!("{base}/{}", draft["id"]),
        Some(writer),
        json!({"draft":false}),
        StatusCode::OK,
    )
    .await;
    assert!(
        published["discussion_url"]
            .as_str()
            .is_some_and(|url| url.contains("/alice/public/discussions/")),
        "{published}"
    );
}

#[tokio::test]
async fn sqlite_github_releases() {
    releases_contract(false).await;
}

#[tokio::test]
async fn postgres_github_releases() {
    releases_contract(true).await;
}

#[tokio::test]
async fn sqlite_github_repository_metadata() {
    metadata_contract(false).await;
}

#[tokio::test]
async fn postgres_github_repository_metadata() {
    metadata_contract(true).await;
}

#[tokio::test]
async fn sqlite_github_rest_contents_refs_commits_and_statuses() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_github_rest_contents_refs_commits_and_statuses() {
    contract(true).await;
}
