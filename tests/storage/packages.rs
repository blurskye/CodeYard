//! Registry requests as Docker sends them, against both database engines.
use crate::support::{database::Database, http_client::Client};
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use codeyard::backend::{
    identity::tokens::{self, TokenInput},
    packages,
};
use serde_json::{json, Value};
use tower::ServiceExt;

mod access;
mod ecosystems;
mod pages;
mod rest;

#[allow(clippy::too_many_arguments)]
async fn request(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    media: &str,
    body: Vec<u8>,
    range: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", media);
    if let Some(token) = token {
        request = request.header("authorization", token);
    }
    if let Some(range) = range {
        request = request.header("content-range", range);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

async fn token(f: &Database, router: &Router, user: i64, scope: &str) -> String {
    let session = Client::login(router, f.pool(), user).await;
    tokens::create(
        f.pool(),
        user,
        Some(&session.session),
        TokenInput {
            name: "registry",
            scope,
            expires_at: None,
        },
    )
    .await
    .unwrap()
    .value
}

async fn push_blob(router: &Router, writer: &str, name: &str, bytes: &[u8]) -> String {
    let digest = packages::digest(bytes);
    let (status, headers, body) = request(
        router,
        "POST",
        &format!("/v2/{name}/blobs/uploads/"),
        Some(writer),
        "application/octet-stream",
        vec![],
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let location = headers["location"].to_str().unwrap();
    let mid = bytes.len() / 2;
    let range = (mid > 0).then(|| format!("0-{}", mid - 1));
    let (status, headers, _) = request(
        router,
        "PATCH",
        location,
        Some(writer),
        "application/octet-stream",
        bytes[..mid].to_vec(),
        range.as_deref(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(headers["range"], format!("0-{}", mid.saturating_sub(1)));
    let (status, _, _) = request(
        router,
        "PATCH",
        location,
        Some(writer),
        "application/octet-stream",
        b"bad".to_vec(),
        Some(&format!("{}-{}", mid + 1, mid + 3)),
    )
    .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    if mid > 0 && mid < bytes.len() {
        // Only the last chunk may be under the advertised minimum.
        let (status, _, body) = request(
            router,
            "PATCH",
            location,
            Some(writer),
            "application/octet-stream",
            bytes[mid..mid + 1].to_vec(),
            Some(&format!("{mid}-{mid}")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains("SIZE_INVALID"));
    }
    let (status, _, _) = request(
        router,
        "PUT",
        &format!("{location}?digest={digest}"),
        Some(writer),
        "application/octet-stream",
        bytes[mid..].to_vec(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, headers, body) = request(
        router,
        "GET",
        &format!("/v2/{name}/blobs/{digest}"),
        Some(writer),
        "application/octet-stream",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], digest);
    assert_eq!(body, bytes);
    digest
}

async fn exchange(router: &Router, credential: Option<&str>, scope: &str) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("service", "codeyard");
    for scope in scope.split_ascii_whitespace() {
        query.append_pair("scope", scope);
    }
    let query = query.finish();
    let (status, _, body) = request(
        router,
        "GET",
        &format!("/v2/token?{query}"),
        credential,
        "application/json",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["expires_in"], 300);
    format!("Bearer {}", body["token"].as_str().unwrap())
}

async fn oci(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("registry-owner").await;
    let bob = f.actor("registry-outsider").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let password = token(&f, &router, alice, "write").await;
    let publisher = format!("Bearer {password}");
    let writer = format!(
        "Basic {}",
        STANDARD.encode(format!("registry-owner:{password}"))
    );
    let reader = format!("Bearer {}", token(&f, &router, alice, "read").await);
    let other = format!("Bearer {}", token(&f, &router, bob, "write").await);
    let (status, headers, _) = request(
        &router,
        "GET",
        "/v2/",
        Some(&writer),
        "application/json",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-distribution-api-version"], "registry/2.0");
    let name = "registry-owner/image";
    let (status, headers, _) = request(
        &router,
        "GET",
        "/v2/",
        None,
        "application/json",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(headers["www-authenticate"]
        .to_str()
        .unwrap()
        .starts_with("Bearer realm="));
    let login = writer;
    let writer = exchange(
        &router,
        Some(&login),
        "repository:registry-owner/image:pull,push",
    )
    .await;
    let pull_token = exchange(
        &router,
        Some(&login),
        "repository:registry-owner/image:pull",
    )
    .await;
    let config = b"{\"architecture\":\"amd64\",\"os\":\"linux\",\"rootfs\":{\"type\":\"layers\",\"diff_ids\":[]}}";
    let config_digest = push_blob(&router, &writer, name, config).await;
    let layer = b"container layer content";
    let layer_digest = push_blob(&router, &writer, name, layer).await;
    let empty_digest = push_blob(&router, &writer, name, b"").await;
    let media = "application/vnd.oci.image.manifest.v1+json";
    let manifest = json!({"schemaVersion":2,"mediaType":media,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":config_digest,"size":config.len()},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":layer_digest,"size":layer.len()},{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":empty_digest,"size":0}]}).to_string().into_bytes();
    let digest = packages::digest(&manifest);
    let path = format!("/v2/{name}/manifests/latest");
    let (status, _, body) = request(
        &router,
        "PUT",
        &path,
        Some(&writer),
        media,
        manifest.clone(),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let (status, _, _) = request(
        &router,
        "PUT",
        &path,
        Some(&pull_token),
        media,
        manifest.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for reference in ["latest", &digest] {
        let (status, headers, body) = request(
            &router,
            "GET",
            &format!("/v2/{name}/manifests/{reference}"),
            Some(&reader),
            media,
            vec![],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["docker-content-digest"], digest);
        assert_eq!(body, manifest);
    }
    let (status, _, body) = request(
        &router,
        "GET",
        &format!("/v2/{name}/tags/list"),
        Some(&reader),
        "application/json",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["tags"],
        json!(["latest"])
    );
    for credential in [None, Some(other.as_str())] {
        let (status, _, _) = request(&router, "GET", &path, credential, media, vec![], None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (status, _, _) = request(
        &router,
        "PUT",
        &path,
        Some(&reader),
        media,
        manifest.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = request(
        &router,
        "PUT",
        &path,
        Some(&other),
        media,
        manifest.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = request(&router, "HEAD", &path, Some(&reader), media, vec![], None).await;
    assert_eq!(status, StatusCode::OK);
    for (credential, expected) in [
        (&publisher, StatusCode::FORBIDDEN),
        (&writer, StatusCode::NOT_FOUND),
    ] {
        let (status, _, _) = request(
            &router,
            "POST",
            "/v2/registry-outsider/image/blobs/uploads/",
            Some(credential),
            media,
            vec![],
            None,
        )
        .await;
        assert_eq!(status, expected);
    }
    // Another namespace cannot mount a private package's object by knowing its digest.
    let (status, headers, _) = request(
        &router,
        "POST",
        &format!("/v2/registry-outsider/image/blobs/uploads/?mount={layer_digest}&from={name}"),
        Some(&other),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers["location"].to_str().unwrap();
    let (status, _, _) = request(
        &router,
        "DELETE",
        location,
        Some(&other),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Only a single content-addressed object is stored for identical content.
    push_blob(&router, &other, "registry-outsider/image", layer).await;
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM package_blobs WHERE size=23", &[])
            .await,
        1
    );
    let mount_path =
        format!("/v2/registry-owner/mirror/blobs/uploads/?mount={layer_digest}&from={name}");
    let mount_target = exchange(
        &router,
        Some(&login),
        "repository:registry-owner/mirror:pull,push",
    )
    .await;
    let (status, _, _) = request(
        &router,
        "POST",
        &mount_path,
        Some(&mount_target),
        "application/octet-stream",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED); // A target-only grant cannot read the source.
    let mount_token = exchange(
        &router,
        Some(&login),
        "repository:registry-owner/mirror:pull,push repository:registry-owner/image:pull",
    )
    .await;
    let (status, _, _) = request(
        &router,
        "POST",
        &mount_path,
        Some(&mount_token),
        "application/octet-stream",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, body) = request(
        &router,
        "GET",
        &format!("/v2/registry-owner/mirror/blobs/{layer_digest}"),
        Some(&mount_token),
        "application/octet-stream",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, layer);
    // Interrupted and unreferenced objects are collected; published layers remain.
    f.execute("UPDATE package_blobs SET expires_at=0", &[])
        .await;
    f.execute("UPDATE package_blob_grants SET expires_at=0", &[])
        .await;
    let storage = codeyard::backend::storage::ObjectStore::open(root.path(), None)
        .await
        .unwrap();
    packages::collect(f.pool(), &storage).await.unwrap();
    let (status, _, body) = request(
        &router,
        "GET",
        &format!("/v2/{name}/blobs/{layer_digest}"),
        Some(&reader),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, layer);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM package_blobs", &[]).await, 4);
    // A multi-platform index retains child manifests and their layers across version deletion.
    let index_media = "application/vnd.oci.image.index.v1+json";
    let index = json!({"schemaVersion":2,"mediaType":index_media,"manifests":[{"mediaType":media,"digest":digest,"size":manifest.len(),"platform":{"architecture":"amd64","os":"linux"}}]}).to_string().into_bytes();
    let (status, _, body) = request(
        &router,
        "PUT",
        &format!("/v2/{name}/manifests/multi"),
        Some(&writer),
        index_media,
        index.clone(),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let (status, _, _) = request(
        &router,
        "DELETE",
        &format!("/v2/{name}/manifests/{digest}"),
        Some(&writer),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    f.execute(
        "UPDATE package_versions SET deleted_at=1 WHERE deleted_at IS NOT NULL",
        &[],
    )
    .await;
    f.execute("UPDATE package_blobs SET expires_at=0", &[])
        .await;
    f.execute("UPDATE package_blob_grants SET expires_at=0", &[])
        .await;
    packages::collect(f.pool(), &storage).await.unwrap();
    let parent = f.scalar("SELECT MAX(v.id) FROM package_versions v JOIN packages p ON p.id=v.package_id WHERE p.owner_user_id=$1 AND p.name='image'",&[alice]).await;
    let (status, _, _) = request(
        &router,
        "DELETE",
        &format!("/v2/{name}/manifests/{}", packages::digest(&index)),
        Some(&writer),
        index_media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    packages::collect(f.pool(), &storage).await.unwrap();
    let (status, _, body) = request(
        &router,
        "POST",
        &format!("/api/v3/users/registry-owner/packages/container/image/versions/{parent}/restore"),
        Some(&publisher),
        "application/json",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty());
    let (status, _, _) = request(
        &router,
        "GET",
        &format!("/v2/{name}/manifests/{digest}"),
        Some(&pull_token),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Deletion hides the version, but a restored index still retains all referenced objects.
    let (status, _, body) = request(
        &router,
        "GET",
        &format!("/v2/{name}/blobs/{digest}"),
        Some(&pull_token),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, manifest);
    let (status, _, body) = request(
        &router,
        "GET",
        &format!("/v2/{name}/blobs/{layer_digest}"),
        Some(&pull_token),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, layer);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v2/{name}/blobs/{layer_digest}"))
                .header("authorization", &pull_token)
                .header("range", "bytes=2-8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers()["content-range"],
        format!("bytes 2-8/{}", layer.len())
    );
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap(),
        &layer[2..9]
    );
    // A scoped credential cannot be used in another package or as a forge API token.
    let (status, _, _) = request(
        &router,
        "GET",
        &format!("/v2/registry-outsider/image/blobs/{layer_digest}"),
        Some(&pull_token),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let push_only = exchange(
        &router,
        Some(&login),
        "repository:registry-owner/image:push",
    )
    .await;
    let (status, _, _) = request(
        &router,
        "GET",
        &format!("/v2/{name}/blobs/{layer_digest}"),
        Some(&push_only),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, body) = request(
        &router,
        "GET",
        "/api/v3/users/registry-owner/packages/container/image",
        Some(&pull_token),
        "application/json",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    crate::support::github_spec::assert_github_schema(
        "packages/get-package-for-user",
        401,
        &serde_json::from_slice::<Value>(&body).unwrap(),
    );
    let grant = codeyard::backend::db::packages::registry_token(
        f.pool(),
        &tokens::token_hash(writer.trim_start_matches("Bearer ")),
    )
    .await
    .unwrap()
    .unwrap();
    let package = f
        .scalar(
            "SELECT id FROM packages WHERE owner_user_id=$1 AND name='image'",
            &[alice],
        )
        .await;
    codeyard::backend::db::packages::settings(
        f.pool(),
        package,
        grant.token_id.unwrap().into(),
        "public",
        None,
    )
    .await
    .unwrap();
    let anonymous = exchange(&router, None, "repository:registry-owner/image:pull").await;
    let (status, _, body) = request(
        &router,
        "GET",
        &format!("/v2/{name}/manifests/multi"),
        Some(&anonymous),
        index_media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, index);
    let (status, _, _) = request(
        &router,
        "PUT",
        &path,
        Some(&anonymous),
        media,
        manifest.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    f.execute("UPDATE package_registry_tokens SET expires_at=1", &[])
        .await;
    let (status, _, _) = request(
        &router,
        "GET",
        &path,
        Some(&pull_token),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    f.finish().await;
}

#[tokio::test]
async fn packages_oci_sqlite() {
    oci(false).await;
}
#[tokio::test]
async fn packages_oci_postgres() {
    oci(true).await;
}

#[tokio::test]
async fn packages_reject_truncated_or_corrupt_chunks() {
    let root = tempfile::tempdir().unwrap();
    let storage = codeyard::backend::storage::ObjectStore::open(root.path(), None)
        .await
        .unwrap();
    let bytes = b"original";
    let blob = codeyard::backend::db::packages::Blob {
        digest: packages::digest(bytes),
        size: bytes.len() as i64,
    };
    let key = codeyard::backend::db::packages::object_key(&blob.digest);
    storage
        .put_bytes(
            &codeyard::backend::storage::Upload {
                key: &key,
                bytes: blob.size,
                content_type: "application/octet-stream",
                sha256_hex: &blob.digest[7..],
            },
            bytes.to_vec(),
        )
        .await
        .unwrap();
    let path = root.path().join("objects").join(&key);
    tokio::fs::write(&path, b"x").await.unwrap();
    assert!(tokio::time::timeout(
        std::time::Duration::from_secs(5),
        packages::read(&storage, &blob)
    )
    .await
    .expect("truncated reads must terminate")
    .is_err());
    tokio::fs::write(&path, b"tampered").await.unwrap();
    assert!(packages::read(&storage, &blob).await.is_err());
}
