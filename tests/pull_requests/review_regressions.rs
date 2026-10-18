//! Git pushes over smart HTTP at the branch and tag limit, and branch names that are not ASCII.
//! Each test runs the real server binary and a real Git client, on SQLite and on PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::fake_webui_bundle as bundle;
use crate::support::git_server;
use crate::support::native_database as native;

use codeyard::backend::db;
use codeyard::backend::repositories::lifecycle;
use codeyard::backend::repositories::store::MAX_REPOSITORY_REFS;
use git_server::{git, success, Server};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{path::Path, process::Output};

const LIMIT_MESSAGE: &str = "A repository holds at most 10000 branches and tags";

/// Writes a token for `owner` that can push, stored the way the server looks tokens up.
async fn write_token(pool: &db::DbPool, owner: i64) -> String {
    let token = native::token();
    let stored = hex::encode(Sha256::digest(token.as_bytes()));
    let session = native::token();
    db::users::sessions::create_session(
        pool,
        &session,
        owner,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    db::users::tokens::create_token(pool, owner, &session, "push", &stored, "write", None)
        .await
        .unwrap()
        .unwrap();
    token
}

/// A running server with one owner, one write token and one repository named `name`.
struct Hosted {
    root: tempfile::TempDir,
    server: Server,
    token: String,
    disk_path: String,
}

impl Hosted {
    async fn start(pool: &db::DbPool, url: &str, name: &str) -> Self {
        bundle::install();
        db::node_config::upsert_node_configuration(pool, "public", true)
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let owner = db::users::accounts::create_user(pool, "http_owner", None)
            .await
            .unwrap();
        let repository = lifecycle::provision(
            (pool, None),
            &root.path().join("repos"),
            db::repositories::RepositoryCreationOwner::personal(owner.id),
            name,
            None,
            false,
            None,
        )
        .await
        .unwrap();
        let token = write_token(pool, owner.id).await;
        let server = Server::start(root.path(), url).await;
        Self {
            root,
            server,
            token,
            disk_path: repository.disk_path,
        }
    }

    fn repo_url(&self, name: &str) -> String {
        format!("{}/http_owner/{name}.git", self.server.base)
    }
}

/// Commits an empty change in the client repository.
fn commit(root: &Path, message: &str) {
    success(git(
        root,
        &["-C", "client", "commit", "--allow-empty", "-qm", message],
        None,
    ));
}

/// The client repository's `HEAD`.
fn head(root: &Path) -> String {
    let output = git(root, &["-C", "client", "rev-parse", "HEAD"], None);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Pushes `refspec` from the client repository to `url`, and returns what happened.
fn push(root: &Path, token: &str, url: &str, refspec: &str) -> Output {
    git(root, &["-C", "client", "push", url, refspec], Some(token))
}

/// The commit that a branch of the stored repository points at.
fn tip(disk_path: &str, branch: &str) -> String {
    let repo = git2::Repository::open_bare(disk_path).unwrap();
    let name = format!("refs/heads/{branch}");
    let reference = repo.find_reference(&name).unwrap();
    reference.target().unwrap().to_string()
}

/// Pushes that would create a branch or tag past the limit are refused, and the pusher is told
/// why. Updating or deleting an existing branch still works, and deleting one makes room for
/// exactly one more.
async fn ref_limit_over_http(pool: &db::DbPool, url: &str) {
    let hosted = Hosted::start(pool, url, "capped").await;
    let root = hosted.root.path();
    let repo_url = hosted.repo_url("capped");
    success(git(
        root,
        &["-c", "init.defaultBranch=main", "init", "-q", "client"],
        None,
    ));
    commit(root, "first");
    let first = head(root);
    success(push(root, &hosted.token, &repo_url, "HEAD:refs/heads/main"));

    // `main` is one of the branches and tags that count, so these fill the repository.
    let bare = git2::Repository::open_bare(&hosted.disk_path).unwrap();
    let target = git2::Oid::from_str(&first).unwrap();
    for index in 1..MAX_REPOSITORY_REFS {
        let name = format!("refs/heads/topic-{index}");
        bare.reference(&name, target, false, "fill").unwrap();
    }

    for refspec in ["HEAD:refs/heads/fresh", "HEAD:refs/tags/fresh"] {
        let refused = push(root, &hosted.token, &repo_url, refspec);
        assert!(!refused.status.success(), "{refspec} was accepted");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains(LIMIT_MESSAGE), "{stderr}");
    }
    assert!(bare.find_reference("refs/heads/fresh").is_err());

    commit(root, "second");
    let second = head(root);
    success(push(
        root,
        &hosted.token,
        &repo_url,
        "HEAD:refs/heads/topic-1",
    ));
    success(push(root, &hosted.token, &repo_url, ":refs/heads/topic-2"));
    assert_eq!(tip(&hosted.disk_path, "topic-1"), second);
    assert!(bare.find_reference("refs/heads/topic-2").is_err());

    success(push(
        root,
        &hosted.token,
        &repo_url,
        "HEAD:refs/heads/fresh",
    ));
    assert_eq!(tip(&hosted.disk_path, "fresh"), second);
}

/// A branch whose name is not ASCII pushes over HTTP, and the branch is listed afterwards.
async fn unicode_branch_over_http(pool: &db::DbPool, url: &str) {
    let hosted = Hosted::start(pool, url, "unicode").await;
    let root = hosted.root.path();
    let repo_url = hosted.repo_url("unicode");
    success(git(
        root,
        &["-c", "init.defaultBranch=main", "init", "-q", "client"],
        None,
    ));
    commit(root, "first");
    success(push(root, &hosted.token, &repo_url, "HEAD:refs/heads/main"));

    let branch = "équipe/功能";
    let pushed = head(root);
    let refspec = format!("HEAD:refs/heads/{branch}");
    success(push(root, &hosted.token, &repo_url, &refspec));
    assert_eq!(tip(&hosted.disk_path, branch), pushed);

    let api = format!("{}/api/repos/http_owner/unicode", hosted.server.base);
    let response = Client::new()
        .get(format!("{api}/branches"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let listed: Value = response.json().await.unwrap();
    let names = listed.as_array().unwrap();
    assert!(names.iter().any(|entry| entry["name"] == branch));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ref_limit_over_http_sqlite() {
    let database = native::NativeDatabase::new().await;
    ref_limit_over_http(&database.pool, &database.url).await;
    database.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ref_limit_over_http_postgres() {
    let database = native::PostgresDatabase::new().await;
    ref_limit_over_http(&database.pool, &database.connection_url()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unicode_branch_over_http_sqlite() {
    let database = native::NativeDatabase::new().await;
    unicode_branch_over_http(&database.pool, &database.url).await;
    database.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unicode_branch_over_http_postgres() {
    let database = native::PostgresDatabase::new().await;
    unicode_branch_over_http(&database.pool, &database.connection_url()).await;
}
