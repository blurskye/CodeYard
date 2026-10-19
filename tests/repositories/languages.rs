//! Repository languages are stored rows of the default branch's language mix. A push, web edit,
//! merge or fork that moves the default branch stores them, and the first reader computes them
//! when none are stored. The API, the About panel and the Explore filter read the rows.
#![cfg(not(target_arch = "wasm32"))]
use crate::support::{
    fake_webui_bundle as bundle,
    git_server::{git, success, Server},
    native_database as native,
};

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use chrono::Utc;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db::{self, users::transitions::SetupClaim},
    repositories::lifecycle,
    storage::ObjectStore,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc, time::Duration};
use tower::ServiceExt;

const RUST_CONTENT: &str = "pub fn answer() -> u32 { 42 }\n";
const SHELL_CONTENT: &str = "#!/bin/sh\necho hi\n";
const PYTHON_CONTENT: &str = "print('hi')\n";
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
    // The first reader computes the languages of Git's default branch and stores them.
    let (status, body) = get_json(&router, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body, expected,
        "use Git's default branch, not the database's"
    );
    assert_eq!(
        db::repository_stats::get_repo_languages(pool, repository.id)
            .await
            .unwrap()
            .len(),
        1,
        "the computed languages are stored"
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

    // Stored rows answer without Git, so the repository directory being gone changes nothing.
    std::fs::rename(&disk_path, directory.path().join("unavailable.git")).unwrap();
    let (status, body) = get_json(&router, Some(&session)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, expected, "stored languages are served without Git");

    // Without stored rows the reader has to compute them, and Git is unavailable.
    db::repository_stats::upsert_repo_languages(pool, repository.id, &[])
        .await
        .unwrap();
    let (status, body) = get_json(&router, Some(&session)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.to_string().contains("git_error"), "{body}");
    assert!(!body["message"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn first_language_reader_retries_when_the_default_changes_during_publication() {
    let fixture = native::NativeDatabase::new().await;
    let owner = db::users::accounts::create_user(&fixture.pool, "language_race", None)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let disk = root.path().join("race.git");
    let record = db::repositories::records::create_repository(
        &fixture.pool,
        owner.id,
        "race",
        None,
        false,
        disk.to_str().unwrap(),
    )
    .await
    .unwrap();
    let git = git2::Repository::init_bare(&disk).unwrap();
    let signature = git2::Signature::now("test", "test@example.invalid").unwrap();
    for (branch, name) in [("main", "lib.rs"), ("topic", "app.py")] {
        let mut tree = git.treebuilder(None).unwrap();
        tree.insert(name, git.blob(b"some source\n").unwrap(), 0o100644)
            .unwrap();
        git.commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            "source",
            &git.find_tree(tree.write().unwrap()).unwrap(),
            &[],
        )
        .unwrap();
    }
    git.set_head("refs/heads/main").unwrap();
    let tx = fixture.raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let pool = fixture.pool.clone();
    let store = codeyard::backend::repositories::store::repo_store(&record, None);
    let task = tokio::spawn(async move {
        codeyard::backend::repositories::languages::compute_and_store(
            &pool, &store, record.id, "main",
        )
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !task.is_finished(),
        "publication waits for the database writer"
    );
    git.set_head("refs/heads/topic").unwrap();
    tx.commit().await.unwrap();
    let languages = task.await.unwrap();
    assert_eq!(languages[0].language, "Python");
    let stored = db::repository_stats::get_repo_languages(&fixture.pool, record.id)
        .await
        .unwrap();
    assert_eq!(stored[0].language, "Python");
    fixture.close().await;
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

/// Commits `files` on `branch` of the working copy at `source` and pushes that branch to `url`
/// with `token`. The working copy is created on first use. Its HEAD is switched rather than
/// checked out, so the files of the previous commit stay in place.
fn commit_and_push(
    root: &Path,
    source: &Path,
    branch: &str,
    files: &[(&str, &str)],
    url: &str,
    token: &str,
) {
    let source = source.to_str().unwrap();
    if !Path::new(source).exists() {
        success(git(root, &["init", "-q", source], None));
    }
    success(git(
        root,
        &[
            "-C",
            source,
            "symbolic-ref",
            "HEAD",
            &format!("refs/heads/{branch}"),
        ],
        None,
    ));
    for (name, content) in files {
        std::fs::write(Path::new(source).join(name), content).unwrap();
    }
    success(git(root, &["-C", source, "add", "-A"], None));
    success(git(
        root,
        &[
            "-C",
            source,
            "commit",
            "-q",
            "-m",
            &format!("Add {branch} files"),
        ],
        None,
    ));
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    success(git(
        root,
        &["-C", source, "push", "-q", url, &refspec],
        Some(token),
    ));
}

/// The stored languages of `repo_id`, once a background refresh has written them.
async fn stored_languages(pool: &db::DbPool, repo_id: i64) -> Vec<db::models::RepoLanguage> {
    for _ in 0..100 {
        let rows = db::repository_stats::get_repo_languages(pool, repo_id)
            .await
            .unwrap();
        if !rows.is_empty() {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no languages were stored");
}

/// A page of the in-process router as HTML, with its status.
async fn page(router: &Router, path: &str) -> (StatusCode, String) {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// Pushes to a public repository over smart HTTP, the way a client does, and checks what the
/// database, the repository page and Explore show of the languages it stores.
async fn push_contract(pool: &db::DbPool, database_url: &str) {
    bundle::install();
    const SETUP: &str = "INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE";
    match pool {
        db::DbPool::Postgres(raw) => {
            sqlx::query(SETUP).execute(raw).await.unwrap();
        }
        db::DbPool::Sqlite(raw) => {
            sqlx::query(SETUP).execute(raw).await.unwrap();
        }
    }
    let owner = db::users::accounts::create_user(pool, "language_owner", None)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let repository = lifecycle::provision(
        (pool, None),
        &root.path().join("repos"),
        db::repositories::creation::RepositoryCreationOwner::personal(owner.id),
        "public-languages",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let session = native::token();
    db::users::sessions::create_session(
        pool,
        &session,
        owner.id,
        None,
        None,
        Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let token = native::token();
    db::users::tokens::create_token(
        pool,
        owner.id,
        &session,
        "push",
        &hex::encode(Sha256::digest(token.as_bytes())),
        "write",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let server = Server::start(root.path(), database_url).await;
    let push_url = format!("{}/language_owner/public-languages.git", server.base);
    let source = root.path().join("source");

    // A push to the default branch stores its languages, largest first.
    commit_and_push(
        root.path(),
        &source,
        "main",
        &[("lib.rs", RUST_CONTENT), ("run.sh", SHELL_CONTENT)],
        &push_url,
        &token,
    );
    let stored = stored_languages(pool, repository.id).await;
    assert_eq!(
        stored
            .iter()
            .map(|row| (row.language.as_str(), row.bytes))
            .collect::<Vec<_>>(),
        [
            ("Rust", RUST_CONTENT.len() as i64),
            ("Shell", SHELL_CONTENT.len() as i64),
        ]
    );
    assert_eq!(stored[0].color.as_deref(), Some("#dea584"));
    let total: f64 = stored.iter().map(|row| row.percentage).sum();
    assert!((total - 100.0).abs() < 0.05, "shares add up: {total}");

    // The repository home shows the bar, and Explore finds the repository by its language.
    let router = create_router(AppState::new(
        pool.clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let (status, home) = page(&router, "/language_owner/public-languages").await;
    assert_eq!(status, StatusCode::OK, "{home}");
    assert!(home.contains("class=\"language-bar\""), "{home}");
    assert!(home.contains("title=\"Rust "), "{home}");
    let (status, explore) = page(&router, "/explore?language=Rust").await;
    assert_eq!(status, StatusCode::OK, "{explore}");
    assert!(
        explore.contains("href=\"/language_owner/public-languages\""),
        "{explore}"
    );
    assert!(explore.contains("value=\"Rust\""), "{explore}");
    assert!(explore.contains("class=\"repo-language\""), "{explore}");
    assert!(explore.contains("background: #dea584"), "{explore}");
    let (status, other) = page(&router, "/explore?language=Go").await;
    assert_eq!(status, StatusCode::OK, "{other}");
    assert!(
        !other.contains("href=\"/language_owner/public-languages\""),
        "{other}"
    );

    // A push to another branch stores nothing, even with no stored languages to replace.
    db::repository_stats::upsert_repo_languages(pool, repository.id, &[])
        .await
        .unwrap();
    commit_and_push(
        root.path(),
        &source,
        "feature",
        &[("app.py", PYTHON_CONTENT)],
        &push_url,
        &token,
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        db::repository_stats::get_repo_languages(pool, repository.id)
            .await
            .unwrap()
            .is_empty(),
        "a push to another branch stores nothing"
    );

    // The next push to the default branch stores the languages again.
    commit_and_push(
        root.path(),
        &source,
        "main",
        &[("more.py", PYTHON_CONTENT)],
        &push_url,
        &token,
    );
    let stored = stored_languages(pool, repository.id).await;
    assert!(
        stored.iter().any(|row| row.language == "Python"),
        "{stored:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_default_branch_pushes_store_languages() {
    let fixture = native::NativeDatabase::new().await;
    push_contract(&fixture.pool, &fixture.url).await;
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_default_branch_pushes_store_languages() {
    let fixture = native::PostgresDatabase::new().await;
    push_contract(&fixture.pool, &fixture.connection_url()).await;
    fixture.pool.close().await;
}
