//! Real ZIP/text responses and delegated log capabilities on each database.
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use serde_json::Value;
use std::io::Read;
use tower::ServiceExt;

async fn request(
    api: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = api
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}
async fn redirect(api: &Router, path: &str, token: &str) -> String {
    let (status, headers, body) = request(api, "GET", path, Some(token)).await;
    assert_eq!(
        status,
        StatusCode::FOUND,
        "{path}: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(body.is_empty());
    headers["location"].to_str().unwrap().to_owned()
}
async fn missing(api: &Router, path: &str) {
    let (status, _, body) = request(api, "GET", path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_github_schema(
        "git/get-blob",
        404,
        &serde_json::from_slice::<Value>(&body).unwrap(),
    );
}

pub(super) async fn contract(
    f: &Database,
    api: &Router,
    base: &str,
    run: i64,
    job: i64,
    reader: &str,
    writer: &str,
) {
    let paths = [
        format!("{base}/runs/{run}/logs"),
        format!("{base}/runs/{run}/attempts/1/logs"),
        format!("{base}/jobs/{job}/logs"),
        // GitHub numbers steps here by their zero-based position in the job.
        format!("{base}/jobs/{job}/steps/0/logs"),
    ];
    let mut last = String::new();
    for (index, path) in paths.iter().enumerate() {
        let location = redirect(api, path, reader).await;
        assert!(location.contains("codeyard_download="));
        let (status, headers, body) = request(api, "GET", &location, None).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(headers["cache-control"], "no-store");
        if index < 2 {
            assert_eq!(headers["content-type"], "application/zip");
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(body)).unwrap();
            let mut text = String::new();
            archive
                .by_name("Build Linux/1_Run build.txt")
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            assert!(text.contains("observed output"));
            let mut whole = String::new();
            archive
                .by_name("0_Build Linux.txt")
                .unwrap()
                .read_to_string(&mut whole)
                .unwrap();
            assert_eq!(whole, text);
            assert!(archive.file_names().all(|name| name.is_ok_and(
                |name| !name.starts_with('/') && !name.split('/').any(|part| part == "..")
            )));
        } else {
            assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
            assert!(String::from_utf8(body).unwrap().contains("observed output"));
        }
        missing(api, &location.replace("/project/", "/other/")).await;
        last = location;
    }
    // A valid delegated URL must refuse object bytes that no longer match SQL metadata.
    f.execute(
        "UPDATE ci_log_chunks SET object_digest='0000000000000000000000000000000000000000000000000000000000000000' WHERE run_id=$1",
        &[run],
    ).await;
    let (status, _, body) = request(api, "GET", &last, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!String::from_utf8_lossy(&body).contains("observed output"));
    missing(api, &last.replace("/steps/0/", "/steps/1/")).await;
    f.execute(
        "UPDATE actions_log_downloads SET expires_at=0 WHERE run_id=$1",
        &[run],
    )
    .await;
    missing(api, &last).await;
    let fresh = redirect(api, &paths[0], reader).await;
    super::actions_workflows::checked(
        api,
        "actions/delete-workflow-run-logs",
        "DELETE",
        &format!("{base}/runs/{run}/logs"),
        writer,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    missing(api, &fresh).await;
    let (status, _, body) = request(api, "GET", &paths[2], Some(reader)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_github_schema(
        "git/get-blob",
        404,
        &serde_json::from_slice::<Value>(&body).unwrap(),
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM ci_log_chunks WHERE run_id=$1", &[run])
            .await,
        0
    );
}
