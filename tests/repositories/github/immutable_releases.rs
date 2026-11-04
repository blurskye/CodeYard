use super::*;
use crate::support::git_server;
use crate::support::git_server::Server;
use sha2::Digest;
mod namespaces;
async fn immutable_contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
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
        json!({"name":"locked","auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let repo = f.repository(created["id"].as_i64().unwrap()).await;
    let base = "/api/v3/repos/alice/locked";
    let endpoint = format!("{base}/immutable-releases");
    let (_, old) = checked(
        &router,
        "repos/create-release",
        "POST",
        &format!("{base}/releases"),
        Some(&writer),
        json!({"tag_name":"v0","target_commitish":"main"}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(old["immutable"], false);
    checked(
        &router,
        "repos/check-immutable-releases",
        "GET",
        &endpoint,
        Some(&reader),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "repos/enable-immutable-releases",
        "PUT",
        &endpoint,
        Some(&reader),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/enable-immutable-releases",
        "PUT",
        &endpoint,
        Some(&outsider),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/enable-immutable-releases",
        "PUT",
        &endpoint,
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let (_, policy) = checked(
        &router,
        "repos/check-immutable-releases",
        "GET",
        &endpoint,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(policy, json!({"enabled":true,"enforced_by_owner":false}));
    let (_, old) = checked(
        &router,
        "repos/update-release",
        "PATCH",
        &format!("{base}/releases/{}", old["id"]),
        Some(&writer),
        json!({"body":"Existing publications stay mutable"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(old["immutable"], false);
    let owner = Client::login(&router, f.pool(), alice).await;
    let settings = owner.page_ok("/alice/locked/settings?tab=releases").await;
    assert!(settings.contains("Immutable releases"));
    assert!(settings.contains("permanently reserve"));
    let (_, draft) = checked(
        &router,
        "repos/create-release",
        "POST",
        &format!("{base}/releases"),
        Some(&writer),
        json!({"tag_name":"v1","target_commitish":"main","draft":true}),
        StatusCode::CREATED,
    )
    .await;
    let id = draft["id"].as_i64().unwrap();
    assert_eq!(draft["immutable"], false);
    let upload = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{base}/releases/{id}/assets?name=bundle.txt"))
                .header("authorization", &writer)
                .header("content-type", "text/plain")
                .body(Body::from("release artifact\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::CREATED);
    let bytes = to_bytes(upload.into_body(), 1024 * 1024).await.unwrap();
    let asset: Value = serde_json::from_slice(&bytes).unwrap();
    assert_github_schema("repos/upload-release-asset", 201, &asset);
    let asset_id = asset["id"].as_i64().unwrap();
    let (_, published) = checked(
        &router,
        "repos/update-release",
        "PATCH",
        &format!("{base}/releases/{id}"),
        Some(&writer),
        json!({"draft":false}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(published["immutable"], true);
    // Exercise the real receive hook, not a simulated ref update.
    let server = Server::start(root.path(), &f.connection_url()).await;
    let token = git_server::write_token(f.pool(), alice).await;
    let remote = format!("{}/alice/locked.git", server.base);
    git_server::success(git_server::git(
        root.path(),
        &["clone", "-q", &remote, "immutable-client"],
        Some(&token),
    ));
    let work = root.path().join("immutable-client");
    std::fs::write(work.join("changed.txt"), "new commit\n").unwrap();
    git_server::success(git_server::git(&work, &["add", "changed.txt"], None));
    git_server::success(git_server::git(
        &work,
        &["commit", "-q", "-m", "New content"],
        None,
    ));
    git_server::success(git_server::git(&work, &["tag", "-f", "v1"], None));
    let changed = git_server::git(
        &work,
        &["push", "--force", "origin", "refs/tags/v1"],
        Some(&token),
    );
    assert!(!changed.status.success(), "{changed:?}");
    assert!(
        String::from_utf8_lossy(&changed.stderr).contains("immutable"),
        "{changed:?}"
    );
    let deleted = git_server::git(&work, &["push", "origin", ":refs/tags/v1"], Some(&token));
    assert!(!deleted.status.success(), "{deleted:?}");
    let page = owner.page_ok("/alice/locked/releases/tag/v1").await;
    assert!(page.contains("Immutable"));
    assert!(page.contains("Download signed release attestation"));
    let editor = owner
        .page_ok(&format!("/alice/locked/releases/edit/{id}"))
        .await;
    assert!(!editor.contains("Upload asset"));
    assert!(!editor.contains("Edit asset"));
    let refused = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{base}/releases/{id}/assets?name=late.txt"))
                .header("authorization", &writer)
                .header("content-type", "text/plain")
                .body(Body::from("too late\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bytes = to_bytes(refused.into_body(), 1024 * 1024).await.unwrap();
    let refusal: Value = serde_json::from_slice(&bytes).unwrap();
    assert_github_schema("repos/upload-release-asset", 422, &refusal);
    let proof = request(
        &router,
        "GET",
        &format!("/alice/locked/releases/{id}/attestation"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(proof.0, StatusCode::OK);
    assert_eq!(proof.2["payloadType"], "application/vnd.in-toto+json");
    use base64::Engine;
    let signed = base64::engine::general_purpose::STANDARD
        .decode(proof.2["payload"].as_str().unwrap())
        .unwrap();
    let signed: Value = serde_json::from_slice(&signed).unwrap();
    assert_eq!(signed["predicate"]["tag"], "v1");
    assert_eq!(signed["predicate"]["assets"][0]["name"], "bundle.txt");
    assert_eq!(
        signed["predicate"]["assets"][0]["sha256"],
        format!("{:x}", sha2::Sha256::digest(b"release artifact\n"))
    );
    checked(
        &router,
        "repos/update-release-asset",
        "PATCH",
        &format!("{base}/releases/assets/{asset_id}"),
        Some(&writer),
        json!({"name":"replaced.txt"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &router,
        "repos/delete-release-asset",
        "DELETE",
        &format!("{base}/releases/assets/{asset_id}"),
        Some(&writer),
        Value::Null,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &router,
        "repos/update-release",
        "PATCH",
        &format!("{base}/releases/{id}"),
        Some(&writer),
        json!({"draft":true}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let (_, edited) = checked(
        &router,
        "repos/update-release",
        "PATCH",
        &format!("{base}/releases/{id}"),
        Some(&writer),
        json!({"name":"Editable title","body":null,"prerelease":true}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(edited["immutable"], true);
    assert_eq!(edited["body"], Value::Null);
    let store = codeyard_app::repositories::store::repo_store(&repo, None);
    assert!(store
        .delete_tag_as(f.pool(), repo.id, alice, "v1")
        .await
        .is_err());
    checked(
        &router,
        "repos/disable-immutable-releases",
        "DELETE",
        &endpoint,
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert!(store
        .delete_tag_as(f.pool(), repo.id, alice, "v1")
        .await
        .is_err());
    checked(
        &router,
        "repos/check-immutable-releases",
        "GET",
        &endpoint,
        Some(&reader),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "repos/delete-release",
        "DELETE",
        &format!("{base}/releases/{id}"),
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    store
        .delete_tag_as(f.pool(), repo.id, alice, "v1")
        .await
        .unwrap();
    let sha = store.refs_from_primary().await.unwrap()["refs/heads/main"].clone();
    assert!(store
        .create_tag_as(
            f.pool(),
            repo.id,
            alice,
            "v1",
            &sha,
            None,
            "Alice",
            "alice@example.invalid"
        )
        .await
        .is_err());
    checked(
        &router,
        "repos/create-release",
        "POST",
        &format!("{base}/releases"),
        Some(&writer),
        json!({"tag_name":"v1","target_commitish":"main"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
}
#[tokio::test]
async fn sqlite_github_immutable_releases() {
    immutable_contract(false).await;
}
#[tokio::test]
async fn postgres_github_immutable_releases() {
    immutable_contract(true).await;
}
