//! Independent web nodes keep only temporary files on their own disks.
use super::FakeS3;
use crate::support::database::Database;
use crate::support::live_session;
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    response::Response,
    Router,
};

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_ci::runtime;
use codeyard_git_storage::repositories::lifecycle;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use codeyard_storage::ObjectStore;
use serde_json::{json, Value};
use std::{path::Path, sync::Arc, time::Duration};
use tower::ServiceExt;
use uuid::Uuid;

async fn web(database: &Database, root: &Path, s3: &FakeS3) -> Router {
    create_router(AppState::new(
        database.pool().clone(),
        root.to_owned(),
        Arc::new(
            ObjectStore::open(root, Some(s3.config.clone()))
                .await
                .unwrap(),
        ),
        None,
        CiConfig::default(),
        None,
    ))
}

async fn request(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Vec<u8>,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(
            header::CONTENT_TYPE,
            if method == "POST" && path.starts_with("/twirp/") {
                "application/json"
            } else {
                "application/octet-stream"
            },
        )
        .header(header::ACCEPT, "application/octet-stream");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    router
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}

async fn bytes(response: Response, status: StatusCode) -> Vec<u8> {
    let actual = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    assert_eq!(actual, status, "{}", String::from_utf8_lossy(&bytes));
    bytes
}

async fn rpc(router: &Router, token: &str, method: &str, body: &Value) -> Value {
    let response = request(
        router,
        "POST",
        &format!("/twirp/github.actions.results.api.v1.ArtifactService/{method}"),
        Some(token),
        serde_json::to_vec(body).unwrap(),
    )
    .await;
    serde_json::from_slice(&bytes(response, StatusCode::OK).await).unwrap()
}

fn local_url(url: &str) -> String {
    let url = url::Url::parse(url).unwrap();
    format!(
        "{}{}",
        url.path(),
        url.query().map(|q| format!("?{q}")).unwrap_or_default()
    )
}

async fn files_survive_a_web_node(pg: bool) {
    let mut database = Database::new(pg).await;
    database.complete_setup().await;
    let actor = database.actor("alice").await;
    let root1 = tempfile::tempdir().unwrap();
    let root2 = tempfile::tempdir().unwrap();
    let s3 = FakeS3::start("shared-web-fixture").await;
    let repo = lifecycle::provision(
        (database.pool(), None),
        root1.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(actor),
        "proj",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let session = live_session(database.pool(), actor).await;
    let token = tokens::create(
        database.pool(),
        actor,
        Some(&session),
        TokenInput {
            name: "shared files",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap()
    .value;
    let release = codeyard_db_repos::releases::create_release(
        database.pool(),
        actor,
        repo.id,
        "v1",
        "main",
        &codeyard_db_repos::releases::ReleaseFields {
            is_draft: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let router1 = web(&database, root1.path(), &s3).await;
    let release_bytes = b"release bytes from the first web node";
    let uploaded = request(
        &router1,
        "POST",
        &format!(
            "/api/v3/repos/alice/proj/releases/{}/assets?name=release.bin",
            release.id
        ),
        Some(&token),
        release_bytes.to_vec(),
    )
    .await;
    let asset: Value = serde_json::from_slice(&bytes(uploaded, StatusCode::CREATED).await).unwrap();
    let asset_id = asset["id"].as_i64().unwrap();

    let sql = "INSERT INTO ci_runs(run_id,repo_id,triggered_by_id,mode,pipeline,event,branch,workflow_path,status,worker) VALUES($1,$2,$3,'local','ci','push','main','.github/workflows/ci.yml','running','test') RETURNING *";
    let run: codeyard_database::models::CiRun = match database.pool() {
        codeyard_database::DbPool::Postgres(pool) => sqlx::query_as(sql)
            .bind(Uuid::new_v4())
            .bind(repo.id)
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap(),
        codeyard_database::DbPool::Sqlite(pool) => sqlx::query_as(sql)
            .bind(Uuid::new_v4())
            .bind(repo.id)
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap(),
    };
    let runtime = runtime::issue(
        database.pool(),
        &run,
        &codeyard_db_actions::actions::ActionsSettings::default(),
    )
    .await
    .unwrap();
    let mut artifact = json!({
        "workflowRunBackendId": run.run_id.to_string(),
        "workflowJobRunBackendId": run.run_id.to_string(),
        "name": "dist",
        "version": 4,
    });
    let created = rpc(&router1, &runtime.token, "CreateArtifact", &artifact).await;
    let mut zip = vec![0; 22];
    zip[..4].copy_from_slice(b"PK\x05\x06");
    let put = request(
        &router1,
        "PUT",
        &local_url(created["signedUploadUrl"].as_str().unwrap()),
        None,
        zip.clone(),
    )
    .await;
    bytes(put, StatusCode::CREATED).await;
    artifact["size"] = json!(zip.len());
    let finalized = rpc(&router1, &runtime.token, "FinalizeArtifact", &artifact).await;
    let artifact_id = finalized["artifactId"].as_str().unwrap();
    assert!(s3.state.puts.lock().unwrap().len() >= 2);
    assert!(!root1.path().join("objects").exists());
    drop(router1);
    drop(root1);

    // No AppState, ObjectStore or local directory is carried over from node 1.
    let router2 = web(&database, root2.path(), &s3).await;
    let asset_path = format!("/api/v3/repos/alice/proj/releases/assets/{asset_id}");
    let denied = request(&router2, "GET", &asset_path, None, vec![]).await;
    bytes(denied, StatusCode::NOT_FOUND).await;
    let download = request(&router2, "GET", &asset_path, Some(&token), vec![]).await;
    assert_eq!(download.status(), StatusCode::FOUND);
    let location = download.headers()[header::LOCATION].to_str().unwrap();
    let downloaded = reqwest::Client::new()
        .get(location)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .unwrap();
    assert_eq!(downloaded.status(), StatusCode::OK);
    assert_eq!(downloaded.bytes().await.unwrap().as_ref(), release_bytes);

    let artifact_path = format!("/api/v3/repos/alice/proj/actions/artifacts/{artifact_id}/zip");
    let download = request(&router2, "GET", &artifact_path, Some(&token), vec![]).await;
    assert_eq!(download.status(), StatusCode::FOUND);
    let location = local_url(download.headers()[header::LOCATION].to_str().unwrap());
    let download = request(&router2, "GET", &location, None, vec![]).await;
    assert_eq!(bytes(download, StatusCode::OK).await, zip);
    assert!(!root2.path().join("objects").exists());
    drop(router2);
    database.finish().await;
}

#[tokio::test]
async fn sqlite_files_survive_a_web_node() {
    files_survive_a_web_node(false).await;
}

#[tokio::test]
async fn postgres_files_survive_a_web_node() {
    files_survive_a_web_node(true).await;
}

#[tokio::test]
async fn cluster_startup_uses_storage_nodes_without_s3() {
    use crate::support::{git_remote::Server, native_database::PostgresDatabase};
    use sha2::{Digest, Sha256};
    let mut database = PostgresDatabase::new().await;
    crate::support::fake_webui_bundle::install();
    crate::support::native_database::enable_cluster(&database.pool)
        .await
        .unwrap();
    codeyard_db_placement::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let owner =
        codeyard_db_identity::users::accounts::create_user(&database.pool, "node-files", None)
            .await
            .unwrap();
    let session = live_session(&database.pool, owner.id).await;
    let csrf = crate::support::native_database::token();
    assert_eq!(
        codeyard_db_identity::users::sessions::get_or_create_csrf_token(
            &database.pool,
            &session,
            Some(owner.id),
            &csrf,
            3600
        )
        .await
        .unwrap(),
        Some(csrf.clone())
    );
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "").unwrap();
    let server =
        Server::start_without_s3(root.path(), &database, "web-no-s3", "web", owner.id).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let cookie = format!("session_id={session}");
    let bytes = b"cluster file";
    let reservation = client
        .post(format!("{}/api/files", server.base))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .json(
            &json!({"filename":"shared.txt","content_type":"text/plain","file_size":bytes.len(),
            "reason":"other","visibility":"owner","sha256_hex":hex::encode(Sha256::digest(bytes))}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(reservation.status(), StatusCode::OK);
    let reservation: Value = reservation.json().await.unwrap();
    // With no storage members, the node backend must refuse acknowledgement. A local
    // backend would accept these bytes on the web node and fail this assertion.
    let upload = client
        .put(format!(
            "{}{}",
            server.base,
            reservation["upload_url"].as_str().unwrap()
        ))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .header("content-type", "text/plain")
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::SERVICE_UNAVAILABLE);
    let confirmed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM files WHERE confirmed_at IS NOT NULL")
            .fetch_one(&database.raw)
            .await
            .unwrap();
    assert_eq!(confirmed, 0);
    drop(server);
    database.finish().await;
}
