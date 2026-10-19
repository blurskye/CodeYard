//! CODEOWNERS on pull requests: who is asked to review, and the merge wait that a branch rule adds,
//! on SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;

use axum::http::StatusCode;
use base64::Engine;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use database::Database;
use pulls::{text, Client};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

/// `alice/proj`, a private repository. Bob and Carol have write access, Dave can only read, and
/// Erin has no access. Alice owns `src/` with Carol, and `docs/` goes to Bob and Dave.
struct World {
    _database: Database,
    _root: tempfile::TempDir,
    alice: Client,
    bob: Client,
    dave: Client,
    base: String,
}

const OWNERS: &str = "/src/ @alice @carol\n/docs/ @bob @dave\n/self/ @alice\n";

impl World {
    async fn new(pg: bool) -> Self {
        bundle::install();
        let database = Database::new(pg).await;
        database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
        let alice_id = database.actor("alice").await;
        let bob_id = database.actor("bob").await;
        let carol_id = database.actor("carol").await;
        let dave_id = database.actor("dave").await;
        let root = tempfile::tempdir().unwrap();
        let repository = codeyard::backend::repositories::lifecycle::provision(
            (database.pool(), None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
            "proj",
            None,
            true,
            None,
        )
        .await
        .unwrap();
        for (user, level) in [
            (bob_id, db::models::AccessLevel::Write),
            (carol_id, db::models::AccessLevel::Write),
            (dave_id, db::models::AccessLevel::Read),
        ] {
            db::repositories::access::grant_repo_access(
                database.pool(),
                user,
                repository.id,
                level,
            )
            .await
            .unwrap();
        }
        let router = create_router(AppState::new(
            database.pool().clone(),
            root.path().to_owned(),
            Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
            None,
            CiConfig::default(),
            None,
        ));
        let alice = Client::login(&router, database.pool(), alice_id).await;
        let bob = Client::login(&router, database.pool(), bob_id).await;
        let dave = Client::login(&router, database.pool(), dave_id).await;
        let base = alice.save("main", "README.md", "unborn").await;
        let base = put_file(&alice, "main", ".github/CODEOWNERS", OWNERS, &base).await;
        Self {
            _database: database,
            _root: root,
            alice,
            bob,
            dave,
            base,
        }
    }
}

/// Writes `content` to `path` on `branch` and returns the new tip.
async fn put_file(client: &Client, branch: &str, path: &str, content: &str, tip: &str) -> String {
    let content = base64::engine::general_purpose::STANDARD.encode(content);
    let saved = client
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/contents/{path}"),
            json!({"content": content, "message": format!("Add {path}"), "branch": branch, "expected_revision": tip}),
            StatusCode::CREATED,
        )
        .await;
    saved["sha"].as_str().unwrap().to_owned()
}

/// Opens a pull request from `branch` that adds one file per entry of `files` to `base`.
async fn open_pull(alice: &Client, branch: &str, base: &str, files: &[&str]) -> i64 {
    alice.branch(branch, base).await;
    let mut tip = base.to_owned();
    for file in files {
        tip = alice.save(branch, file, &tip).await;
    }
    let pull = alice.open_pull(branch, branch, "main").await;
    pull["number"].as_i64().unwrap()
}

async fn requested(client: &Client, number: i64) -> BTreeSet<String> {
    let value = client
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/pulls/{number}/requested_reviewers"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["username"].as_str().unwrap().to_owned())
        .collect()
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

async fn set_rule(alice: &Client, code_owner_reviews: bool) {
    let saved = alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({
                "pattern": "main",
                "require_pull_request": true,
                "require_code_owner_reviews": code_owner_reviews
            }),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        saved["require_code_owner_reviews"], code_owner_reviews,
        "{saved}"
    );
}

async fn merge_refused(alice: &Client, number: i64, reason: &str) {
    let pull = alice.pull(number).await;
    let error = alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/merge"),
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"]}),
            StatusCode::CONFLICT,
        )
        .await;
    assert_eq!(error["error"], "branch_protection", "{error}");
    assert!(text(&error, "message").contains(reason), "{error}");
    assert_eq!(alice.pull(number).await["state"], "open");
}

async fn code_owners_request_contract(pg: bool) {
    let world = World::new(pg).await;
    // Alice owns src/ too, but she is the author, so she is not asked. Dave can only read.
    let number = open_pull(
        &world.alice,
        "both",
        &world.base,
        &["src/lib.rs", "docs/a.md"],
    )
    .await;
    assert_eq!(
        requested(&world.alice, number).await,
        names(&["bob", "carol"])
    );

    // Only the owners of the changed files are asked.
    let other = open_pull(&world.alice, "more", &world.base, &["docs/b.md"]).await;
    assert_eq!(requested(&world.alice, other).await, names(&["bob"]));

    // A drafted pull request asks nobody until it is ready.
    world.alice.branch("draft", &world.base).await;
    world.alice.save("draft", "src/draft.rs", &world.base).await;
    let draft = world.alice.open_draft("Draft", "draft", "main").await;
    let draft = draft["number"].as_i64().unwrap();
    assert!(requested(&world.alice, draft).await.is_empty());
    world
        .alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{draft}/ready"),
            json!({}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(requested(&world.alice, draft).await, names(&["carol"]));

    // A new CODEOWNERS on the base branch reaches the pull requests that are already open.
    let content = base64::engine::general_purpose::STANDARD
        .encode("/src/ @alice @carol\n/docs/ @bob @dave @carol\n");
    world
        .alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/contents/.github/CODEOWNERS",
            json!({"content": content, "message": "Add carol to docs", "branch": "main", "expected_revision": world.base}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        requested(&world.alice, other).await,
        names(&["bob", "carol"])
    );
}

async fn merge_gate_contract(pg: bool) {
    let world = World::new(pg).await;
    let number = open_pull(&world.alice, "docs", &world.base, &["docs/a.md"]).await;
    // Only Bob can write to docs/ among its owners, so only Bob's approval counts.
    set_rule(&world.alice, true).await;
    merge_refused(&world.alice, number, "Waiting on review from @bob").await;

    // Dave owns docs/ but cannot vote, so his approval is refused and Bob's is what counts.
    let (status, _) = world
        .dave
        .api(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/reviews"),
            json!({"state": "approved"}),
        )
        .await;
    assert_ne!(status, StatusCode::CREATED);
    world
        .bob
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/reviews"),
            json!({"state": "approved"}),
            StatusCode::CREATED,
        )
        .await;
    let pull = world.alice.pull(number).await;
    world
        .alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/merge"),
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"]}),
            StatusCode::OK,
        )
        .await;

    // With the rule off, a pull request that nobody approved merges.
    set_rule(&world.alice, false).await;
    let open = open_pull(&world.alice, "plain", &world.base, &["docs/c.md"]).await;
    world.alice.merge_current(open).await;
    assert_eq!(world.alice.pull(open).await["state"], "merged");
}

async fn owner_regressions_contract(pg: bool) {
    let world = World::new(pg).await;
    world.alice.branch("reader", &world.base).await;
    world
        .alice
        .save("reader", "docs/reader.md", &world.base)
        .await;
    let pull = world
        .dave
        .open_pull("Reader contribution", "reader", "main")
        .await;
    let number = pull["number"].as_i64().unwrap();
    assert_eq!(requested(&world.alice, number).await, names(&["bob"]));
    world
        .dave
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/requested_reviewers"),
            json!({"username": "carol"}),
            StatusCode::NOT_FOUND,
        )
        .await;

    world.alice.branch("reader-draft", &world.base).await;
    world
        .alice
        .save("reader-draft", "docs/draft.md", &world.base)
        .await;
    let draft = world.dave.open_draft("Draft", "reader-draft", "main").await;
    let draft = draft["number"].as_i64().unwrap();
    assert!(requested(&world.alice, draft).await.is_empty());
    world
        .dave
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{draft}/ready"),
            json!({}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(requested(&world.alice, draft).await, names(&["bob"]));

    world.alice.branch("release", &world.base).await;
    world.alice.expect("PUT", "/api/repos/alice/proj/contents/.github/CODEOWNERS",
        json!({"branch": "release", "expected_revision": world.base, "message": "Release owners",
            "content": base64::engine::general_purpose::STANDARD.encode("/docs/ @carol\n")}), StatusCode::OK).await;
    let pull = world.alice.pull(number).await;
    world.alice.expect("PUT", &format!("/api/repos/alice/proj/pulls/{number}"),
        json!({"expected_state": "open", "expected_updated": pull["updated_at"], "base": "release"}), StatusCode::OK).await;
    assert_eq!(
        requested(&world.alice, number).await,
        names(&["bob", "carol"])
    );

    let own = open_pull(&world.alice, "self-owned", &world.base, &["self/a.txt"]).await;
    assert!(requested(&world.alice, own).await.is_empty());
    world.alice.expect("PUT", "/api/repos/alice/proj/branch-protection",
        json!({"pattern": "main", "require_pull_request": true, "require_code_owner_reviews": true,
            "enforce_admins": true}), StatusCode::OK).await;
    merge_refused(&world.alice, own, "Waiting on review from @alice").await;

    let pool = world._database.pool();
    let repo = db::repositories::records::get_repository(pool, "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let users = db::users::accounts::get_users_by_usernames(
        pool,
        &["alice".into(), "bob".into(), "carol".into(), "dave".into()],
    )
    .await
    .unwrap();
    let ids: Vec<i64> = users.iter().map(|user| user.id).collect();
    for restricted in [false, true] {
        if restricted {
            world
                ._database
                .execute(
                    "UPDATE users SET is_banned = TRUE WHERE username = 'bob'",
                    &[],
                )
                .await;
        }
        let writers = db::repositories::access::write_access_user_ids(pool, repo.id, &ids)
            .await
            .unwrap();
        let current = db::users::accounts::get_users_by_ids(pool, &ids)
            .await
            .unwrap();
        for user in current {
            assert_eq!(
                writers.contains(&user.id),
                codeyard::backend::repositories::access::can_write(pool, &user, &repo)
                    .await
                    .unwrap()
            );
        }
    }
}

#[tokio::test]
async fn sqlite_code_owner_authority_retarget_and_self_ownership() {
    owner_regressions_contract(false).await;
}
#[tokio::test]
async fn postgres_code_owner_authority_retarget_and_self_ownership() {
    owner_regressions_contract(true).await;
}

#[tokio::test]
async fn sqlite_code_owners_are_asked_to_review() {
    code_owners_request_contract(false).await;
}

#[tokio::test]
async fn postgres_code_owners_are_asked_to_review() {
    code_owners_request_contract(true).await;
}

#[tokio::test]
async fn sqlite_code_owner_rule_waits_for_an_owner_approval() {
    merge_gate_contract(false).await;
}

#[tokio::test]
async fn postgres_code_owner_rule_waits_for_an_owner_approval() {
    merge_gate_contract(true).await;
}
