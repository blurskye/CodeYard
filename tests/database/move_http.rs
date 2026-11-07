//! The real HTTP write path must obey a move's database freeze, including rollback.
use crate::support::native_database::PostgresDatabase;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use codeyard_db_cluster::moves::{self, Account};
use codeyard_model::cells::ObjectKind;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

async fn request(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn http_writes_obey_the_freeze_and_resume_after_a_cancel() {
    crate::support::fake_webui_bundle::install();
    let mut database = PostgresDatabase::new().await;
    codeyard_db_cluster::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let user = codeyard_db_identity::users::accounts::create_user(&database.pool, "moving", None)
        .await
        .unwrap();
    let session = codeyard_identity::sessions::fresh_id(&database.pool).unwrap();
    codeyard_db_identity::users::sessions::create_session(
        &database.pool,
        &session,
        user.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let token = database
        .pool
        .cell_prefix()
        .token(codeyard_support::tokens::PAT_PREFIX, &"e".repeat(64))
        .unwrap();
    codeyard_db_identity::users::tokens::create_token(
        &database.pool,
        user.id,
        &session,
        "moves",
        &codeyard_support::token_hash::token_hash(&token),
        "write",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo.git");
    git2::Repository::init_bare(&repo).unwrap();
    codeyard_db_repos::repositories::records::create_repository(
        &database.pool,
        user.id,
        "repo",
        None,
        false,
        repo.to_str().unwrap(),
    )
    .await
    .unwrap();
    let store = Arc::new(
        codeyard_storage::ObjectStore::open(root.path(), None)
            .await
            .unwrap(),
    );
    let app = codeyard_api_rest::create_router(codeyard_app::app::AppState::new(
        database.pool.clone(),
        root.path().into(),
        store,
        None,
        Default::default(),
        None,
    ));
    let path = "/api/v3/repos/moving/repo/issues";
    let (status, issue) =
        request(&app, &token, "POST", path, json!({"title":"Before freeze"})).await;
    assert_eq!(status, StatusCode::CREATED, "{issue}");
    let account = Account::new(ObjectKind::User, user.id).unwrap();
    let wait = Duration::from_secs(10);
    moves::begin(&database.raw, account, 2, wait).await.unwrap();
    moves::freeze(&database.raw, account, Duration::from_secs(60), wait)
        .await
        .unwrap();
    let (status, refusal) =
        request(&app, &token, "POST", path, json!({"title":"Must not land"})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{refusal}");
    let (status, issues) = request(&app, &token, "GET", path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{issues}");
    assert_eq!(issues.as_array().unwrap().len(), 1, "{issues}");
    assert!(moves::cancel(&database.raw, account).await.unwrap());
    let (status, after) =
        request(&app, &token, "POST", path, json!({"title":"After cancel"})).await;
    assert_eq!(status, StatusCode::CREATED, "{after}");
    let (status, issues) = request(&app, &token, "GET", path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{issues}");
    assert_eq!(issues.as_array().unwrap().len(), 2, "{issues}");
    database.finish().await;
}
