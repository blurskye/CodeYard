use super::super::{assert_github_schema, request};
use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use std::io::Cursor;
use tower::ServiceExt;

fn archive(contents: &[u8]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file(
        "database/codeql-database.yml",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    zip.write_all(contents).unwrap();
    zip.finish().unwrap().into_inner()
}

async fn raw(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    media: &str,
    bytes: Vec<u8>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("accept", media)
        .header("content-type", media);
    if let Some(token) = token {
        builder = builder.header("authorization", token);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(bytes)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 101 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

pub(super) async fn workflow_upload(router: &Router, token: &str, expected: u16) {
    let (status, _, body) = raw(
        router,
        "POST",
        "/api/uploads/repos/alice/scan/code-scanning/codeql/databases/python?name=workflow.zip",
        Some(&format!("Bearer {token}")),
        "application/zip",
        archive(b"primaryLanguage: python\n"),
    )
    .await;
    assert_eq!(
        status.as_u16(),
        expected,
        "{}",
        String::from_utf8_lossy(&body)
    );
}

/// The objects due for deletion once the seven-day wait for replaced and deleted objects
/// has passed. The wait is moved into the past rather than slept through.
async fn due_after_wait(f: &Database) -> Vec<String> {
    f.execute(
        "UPDATE pending_object_deletions SET delete_after='2000-01-01T00:00:00.000000Z'",
        &[],
    )
    .await;
    codeyard_db_repos::files::due_object_deletions(f.pool(), 100)
        .await
        .unwrap()
}

async fn contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("codeql-owner").await;
    let bob = f.actor("codeql-outsider").await;
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
                name: "codeql",
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let (writer, reader, outsider) = (&credentials[0], &credentials[1], &credentials[2]);
    call(
        &router,
        "POST",
        "/api/v3/user/repos",
        writer,
        json!({"name":"scan","private":true,"auto_init":true}),
        "repos/create-for-authenticated-user",
        201,
    )
    .await;
    let base = "/api/v3/repos/codeql-owner/scan/code-scanning/codeql/databases";
    let uploads = "/api/uploads/repos/codeql-owner/scan/code-scanning/codeql/databases";
    let repo =
        codeyard_db_repos::repositories::records::get_repository(f.pool(), "codeql-owner", "scan")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        call(
            &router,
            "GET",
            base,
            reader,
            Value::Null,
            "code-scanning/list-codeql-databases",
            200
        )
        .await,
        json!([])
    );
    call(
        &router,
        "GET",
        &format!("{base}/python"),
        reader,
        Value::Null,
        "code-scanning/get-codeql-database",
        404,
    )
    .await;
    let first = archive(b"primaryLanguage: python\n");
    for (token, status) in [
        (Some(reader.as_str()), 403),
        (Some(outsider.as_str()), 404),
        (None, 401),
    ] {
        let (actual, _, body) = raw(
            &router,
            "POST",
            &format!("{uploads}/python?name=python.zip"),
            token,
            "application/zip",
            first.clone(),
        )
        .await;
        assert_eq!(
            actual.as_u16(),
            status,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(body["documentation_url"].is_string());
    }
    for (suffix, media, bytes, status) in [
        ("python", "application/zip", first.clone(), 422),
        ("unknown?name=db.zip", "application/zip", first.clone(), 422),
        (
            "python?name=..%2Fdb.zip",
            "application/zip",
            first.clone(),
            422,
        ),
        (
            "python?name=db.zip",
            "application/octet-stream",
            first.clone(),
            415,
        ),
        (
            "python?name=db.zip",
            "application/zip",
            b"not a zip".to_vec(),
            422,
        ),
    ] {
        let (actual, _, body) = raw(
            &router,
            "POST",
            &format!("{uploads}/{suffix}"),
            Some(writer),
            media,
            bytes,
        )
        .await;
        assert_eq!(
            actual.as_u16(),
            status,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }
    let (status, _, body) = raw(
        &router,
        "POST",
        &format!("{uploads}/python?name=python.zip"),
        Some(writer),
        "application/zip",
        first.clone(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let uploaded: Value = serde_json::from_slice(&body).unwrap();
    assert_github_schema("code-scanning/get-codeql-database", 200, &uploaded);
    assert_eq!(uploaded["uploader"]["id"], alice);
    assert_eq!(uploaded["size"], first.len());
    let old = codeyard_db_security::codeql::get(f.pool(), alice, repo.id, "python")
        .await
        .unwrap()
        .unwrap();
    let metadata = call(
        &router,
        "GET",
        &format!("{base}/python"),
        reader,
        Value::Null,
        "code-scanning/get-codeql-database",
        200,
    )
    .await;
    assert_eq!(metadata, uploaded);
    let list = call(
        &router,
        "GET",
        base,
        reader,
        Value::Null,
        "code-scanning/list-codeql-databases",
        200,
    )
    .await;
    assert_eq!(list, json!([uploaded]));
    let (status, headers, body) = raw(
        &router,
        "GET",
        &format!("{base}/python"),
        Some(reader),
        "application/zip",
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/zip");
    assert_eq!(headers["cache-control"], "private, no-store");
    assert_eq!(body, first);
    for path in [base.to_owned(), format!("{base}/python")] {
        let operation = if path == base {
            "code-scanning/list-codeql-databases"
        } else {
            "code-scanning/get-codeql-database"
        };
        call(&router, "GET", &path, outsider, Value::Null, operation, 404).await;
    }
    let (status, _, _) = raw(
        &router,
        "GET",
        &format!("{base}/python"),
        Some(outsider),
        "application/zip",
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    call(
        &router,
        "DELETE",
        &format!("{base}/python"),
        reader,
        Value::Null,
        "code-scanning/delete-codeql-database",
        403,
    )
    .await;
    call(
        &router,
        "DELETE",
        &format!("{base}/python"),
        outsider,
        Value::Null,
        "code-scanning/delete-codeql-database",
        404,
    )
    .await;
    // Replacing a language publishes one new object and queues the old object for cleanup.
    let second = archive(b"primaryLanguage: python\nversion: 2\n");
    let (status, _, body) = raw(
        &router,
        "POST",
        &format!("{base}/python?name=replaced.zip"),
        Some(writer),
        "application/zip",
        second.clone(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let replaced: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(replaced["id"], uploaded["id"]);
    assert_eq!(replaced["created_at"], uploaded["created_at"]);
    assert_eq!(replaced["name"], "replaced.zip");
    let current = codeyard_db_security::codeql::get(f.pool(), alice, repo.id, "python")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(current.object_key, old.object_key);
    assert!(
        !codeyard_db_repos::files::due_object_deletions(f.pool(), 100)
            .await
            .unwrap()
            .contains(&old.object_key)
    );
    let pending = due_after_wait(&f).await;
    assert!(pending.contains(&old.object_key));
    assert!(!pending.contains(&current.object_key));
    let (_, _, body) = raw(
        &router,
        "GET",
        &format!("{base}/python"),
        Some(reader),
        "application/zip",
        vec![],
    )
    .await;
    assert_eq!(body, second);
    // The uploads-host spelling is also accepted at its unprefixed path.
    let (status, _, body) = raw(
        &router,
        "POST",
        "/repos/codeql-owner/scan/code-scanning/codeql/databases/java?name=java.zip",
        Some(writer),
        "application/zip",
        archive(b"primaryLanguage: java\n"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let list = call(
        &router,
        "GET",
        base,
        reader,
        Value::Null,
        "code-scanning/list-codeql-databases",
        200,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    // Each upload, a replacement included, counts against the uploader's quota.
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM upload_rate_limit WHERE reason='codeql_database' AND user_id=$1",
            &[alice],
        )
        .await,
        3
    );
    f.execute(
        "UPDATE file_reason_limits SET max_uploads_per_hour=3 WHERE reason='codeql_database'",
        &[],
    )
    .await;
    let (status, _, body) = raw(
        &router,
        "POST",
        &format!("{uploads}/python?name=again.zip"),
        Some(writer),
        "application/zip",
        second.clone(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        codeyard_db_security::codeql::get(f.pool(), alice, repo.id, "python")
            .await
            .unwrap()
            .unwrap()
            .object_key,
        current.object_key
    );
    call(
        &router,
        "DELETE",
        &format!("{base}/python"),
        writer,
        Value::Null,
        "code-scanning/delete-codeql-database",
        204,
    )
    .await;
    assert!(due_after_wait(&f).await.contains(&current.object_key));
    call(
        &router,
        "DELETE",
        &format!("{base}/python"),
        writer,
        Value::Null,
        "code-scanning/delete-codeql-database",
        404,
    )
    .await;
    call(
        &router,
        "GET",
        &format!("{base}/python"),
        reader,
        Value::Null,
        "code-scanning/get-codeql-database",
        404,
    )
    .await;
    let java = codeyard_db_security::codeql::get(f.pool(), alice, repo.id, "java")
        .await
        .unwrap()
        .unwrap();
    call(
        &router,
        "DELETE",
        "/api/v3/repos/codeql-owner/scan",
        writer,
        Value::Null,
        "repos/delete",
        204,
    )
    .await;
    assert!(due_after_wait(&f).await.contains(&java.object_key));
}

#[tokio::test]
async fn sqlite_codeql_databases_http_contract() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_codeql_databases_http_contract() {
    contract(true).await;
}

#[tokio::test]
async fn code_scanning_unsupported_groups_return_github_not_found() {
    let f = Database::new(false).await;
    f.complete_setup().await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    for path in [
        "/api/v3/enterprises/example/code-security/configurations",
        "/api/v3/repos/example/repo/code-scanning/ai-scan",
        "/api/v3/repos/example/repo/code-scanning/codeql/variant-analyses/1",
    ] {
        let (status, _, body) = request(&router, "GET", path, None, Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["message"], "Not Found");
        assert!(body["documentation_url"]
            .as_str()
            .unwrap()
            .starts_with("https://docs.github.com/rest/"));
    }
}
