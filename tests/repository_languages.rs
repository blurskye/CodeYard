//! Repository languages come from committed Git files, not cached metadata.
#![cfg(not(target_arch = "wasm32"))]
#[path = "support/native_database.rs"]
mod native;

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db::{self, users::transitions::SetupClaim},
    storage::ObjectStore,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const RUST_CONTENT: &str = "pub fn answer() -> u32 { 42 }\n";
const LANGUAGES_PATH: &str = "/api/repos/language_owner/languages/languages";

async fn get_json(router: &Router, session: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .uri(LANGUAGES_PATH)
        .header(header::ACCEPT, "application/json");
    if let Some(session) = session {
        request = request.header(header::COOKIE, format!("session_id={session}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn languages_contract(pool: &db::DbPool) {
    let owner = match db::users::transitions::complete_setup(
        pool,
        "language_owner",
        None,
        "fixture-hash",
        "public",
        None,
    )
    .await
    .unwrap()
    {
        SetupClaim::Completed(user) => *user,
        other => panic!("unexpected setup outcome: {other:?}"),
    };
    let directory = tempfile::tempdir().unwrap();
    let disk_path = directory.path().join("languages.git");
    let repository = db::repositories::records::create_repository(
        pool,
        owner.id,
        "languages",
        None,
        false,
        disk_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    git2::Repository::init_bare(&disk_path)
        .unwrap()
        .set_head("refs/heads/main")
        .unwrap();
    let router = create_router(AppState::new(
        pool.clone(),
        directory.path().to_owned(),
        Arc::new(ObjectStore::open(directory.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));

    let (status, body) = get_json(&router, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "an empty repository has no languages");

    {
        let git = git2::Repository::open_bare(&disk_path).unwrap();
        for (branch, file, content) in [
            ("main", "decoy.py", "print('decoy')\n"),
            ("trunk", "lib.rs", RUST_CONTENT),
        ] {
            let mut builder = git.treebuilder(None).unwrap();
            builder
                .insert(file, git.blob(content.as_bytes()).unwrap(), 0o100644)
                .unwrap();
            let tree = git.find_tree(builder.write().unwrap()).unwrap();
            let author = git2::Signature::now("fixture", "fixture@example.invalid").unwrap();
            git.commit(
                Some(&format!("refs/heads/{branch}")),
                &author,
                &author,
                "Add language fixture",
                &tree,
                &[],
            )
            .unwrap();
        }
        git.set_head("refs/heads/trunk").unwrap();
    }
    assert_eq!(repository.default_branch, "main");
    assert!(
        db::repository_stats::get_repo_languages(pool, repository.id)
            .await
            .unwrap()
            .is_empty()
    );
    let expected = json!([{
        "language": "Rust",
        "bytes": RUST_CONTENT.len(),
        "percentage": 100.0,
        "color": "#dea584",
    }]);
    let (status, body) = get_json(&router, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body, expected,
        "use Git's default branch, not the database's"
    );

    db::repositories::records::update_repository(
        pool,
        owner.id,
        repository.id,
        None,
        true,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None,
        },
    )
    .await
    .unwrap();
    let (status, body) = get_json(&router, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let session = native::token();
    db::users::sessions::create_session(
        pool,
        &session,
        owner.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let (status, body) = get_json(&router, Some(&session)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body, expected,
        "the private repository's owner can read languages"
    );

    std::fs::rename(&disk_path, directory.path().join("unavailable.git")).unwrap();
    let (status, body) = get_json(&router, Some(&session)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.to_string().contains("git_error"), "{body}");
    assert!(!body["message"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn sqlite_repository_languages_from_git() {
    let fixture = native::NativeDatabase::new().await;
    languages_contract(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_repository_languages_from_git() {
    let fixture = native::PostgresDatabase::new().await;
    languages_contract(&fixture.pool).await;
    fixture.pool.close().await;
}
