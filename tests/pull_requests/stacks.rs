//! Stacks on SQLite and PostgreSQL, through the JSON API and the pull request page. A pull request
//! joins a stack only when someone asks it to. Once stacked, the JSON gives the order and place,
//! the page draws the map, a stacked pull request cannot change its base, a layer waits for the
//! layers below it, and merging the bottom layer moves only the stacked layers that were based on
//! its head branch.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;

use axum::http::StatusCode;
use codeyard::backend::db::pull_requests::records::{StackJoin, StackRefusal};
use codeyard::backend::repositories::pull_requests::{merge, OperationError};
use codeyard::backend::{api::create_router, app::AppState, db, storage::ObjectStore};
use database::Database;
use pulls::Client;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

/// The application over `root`, and the runtime handles that merges need.
struct Runs {
    router: axum::Router,
    alice: i64,
    bob: i64,
    ci: Arc<codeyard::backend::ci::CiRuns>,
    broadcaster: Arc<codeyard::backend::notifications::NotificationBroadcaster>,
}

async fn application_with_runs(f: &mut Database, root: &Path) -> Runs {
    f.execute(
        "INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE",
        &[],
    )
    .await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root,
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let state = AppState::new(
        f.pool().clone(),
        root.to_owned(),
        Arc::new(ObjectStore::open(root, None).await.unwrap()),
        None,
        codeyard::backend::ci::CiConfig::default(),
        None,
    );
    let ci = state.ci_runs.clone();
    let broadcaster = state.notification_broadcaster.clone();
    Runs {
        router: create_router(state),
        alice: alice_id,
        bob: bob_id,
        ci,
        broadcaster,
    }
}

async fn application(f: &mut Database, root: &Path) -> (axum::Router, i64, i64) {
    let runs = application_with_runs(f, root).await;
    (runs.router, runs.alice, runs.bob)
}

fn number_of(value: &Value) -> i64 {
    value["number"].as_i64().unwrap()
}

/// The `stack_id` of pull request `number` in a list answer.
fn listed_stack_id(listed: &Value, number: i64) -> Value {
    listed
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["number"] == number)
        .unwrap()["stack_id"]
        .clone()
}

/// Asks alice's pull request `number` to join the stack its base branch names.
async fn join(alice: &Client, number: i64) -> (StatusCode, Value) {
    alice
        .api(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/stack"),
            Value::Null,
        )
        .await
}

/// Opening layers on each other's branches does not stack them; joining does, and the order is
/// read from the branches. The page offers the join only to writers, and only while it is allowed.
async fn stacks_join_and_order_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;

    // Three layers, each on the branch of the one below.
    alice.branch("feature-1", &main0).await;
    let f1 = alice.save("feature-1", "one.txt", &main0).await;
    let one = alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &f1).await;
    let f2 = alice.save("feature-2", "two.txt", &f1).await;
    let two = alice.open_pull("Two", "feature-2", "feature-1").await;
    alice.branch("feature-3", &f2).await;
    alice.save("feature-3", "three.txt", &f2).await;
    let three = alice.open_pull("Three", "feature-3", "feature-2").await;
    assert_eq!(two["stack_id"], Value::Null);
    assert_eq!(two["stack"], Value::Null);
    assert_eq!(three["stack_id"], Value::Null);
    let stack_id = one["id"].as_i64().unwrap();

    // Before joining, the page offers the join to writers only.
    let (status, page) = alice.page("/alice/proj/pulls/2").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("/alice/proj/pulls/2/stack"), "{page}");
    assert!(page.contains("This pull request is based on #1"), "{page}");
    let (_, reader) = bob.page("/alice/proj/pulls/2").await;
    assert!(!reader.contains("/alice/proj/pulls/2/stack"), "{reader}");

    // Joining starts the stack with the layer below, and asking again changes nothing.
    let (status, joined) = join(&alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["stack_id"], stack_id);
    assert_eq!(joined["stack"]["size"], 2);
    assert_eq!(joined["stack"]["position"], 2);
    assert_eq!(alice.pull(1).await["stack_id"], stack_id);
    let (status, again) = join(&alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["stack_id"], stack_id);
    assert_eq!(again["stack"]["size"], 2);

    // Once stacked, the page draws the map and no longer offers the join.
    let (_, page) = alice.page("/alice/proj/pulls/2").await;
    assert!(page.contains("Layer 2 of 2"), "{page}");
    assert!(!page.contains("/alice/proj/pulls/2/stack"), "{page}");

    // The third layer joins on top, and the JSON gives the order from the bottom up.
    let (status, joined) = join(&alice, 3).await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["stack"]["size"], 3);
    assert_eq!(joined["stack"]["position"], 3);
    let middle = alice.pull(2).await;
    assert_eq!(middle["stack"]["id"], stack_id);
    assert_eq!(middle["stack"]["base_branch"], "main");
    assert_eq!(middle["stack"]["position"], 2);
    let entries = middle["stack"]["entries"].as_array().unwrap();
    let numbers: Vec<i64> = entries.iter().map(number_of).collect();
    assert_eq!(numbers, vec![1, 2, 3]);
    assert_eq!(entries[1]["head_branch"], "feature-2");
    assert_eq!(entries[1]["base_branch"], "feature-1");
    assert_eq!(entries[1]["draft"], false);
    assert_eq!(entries[1]["merged"], false);

    // A pull request on trunk, and the list's stack ids: stacked rows carry the id, others null.
    alice.branch("solo", &main0).await;
    alice.save("solo", "solo.txt", &main0).await;
    let solo = alice.open_pull("Solo", "solo", "main").await;
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls?state=all",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed_stack_id(&listed, 1), stack_id);
    assert_eq!(listed_stack_id(&listed, 2), stack_id);
    assert_eq!(listed_stack_id(&listed, 3), stack_id);
    assert_eq!(listed_stack_id(&listed, number_of(&solo)), Value::Null);

    // The page's Unstack form takes the stack apart; each layer keeps its base.
    let reply = alice.form_reply("/alice/proj/pulls/2/unstack", &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(alice.pull(1).await["stack_id"], Value::Null);
    assert_eq!(alice.pull(2).await["stack_id"], Value::Null);
    assert_eq!(alice.pull(3).await["stack"], Value::Null);
    assert_eq!(alice.pull(2).await["base_branch"], "feature-1");
    assert_eq!(alice.pull(3).await["base_branch"], "feature-2");

    // With the stack gone, the join is offered again.
    let (_, page) = alice.page("/alice/proj/pulls/2").await;
    assert!(page.contains("/alice/proj/pulls/2/stack"), "{page}");
    f.finish().await;
}

/// Joins that the rules refuse, and base changes of stacked pull requests.
async fn stacks_join_refusals_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("develop", &main0).await;
    alice.branch("feature-1", &main0).await;
    let f1 = alice.save("feature-1", "one.txt", &main0).await;
    let one = alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &f1).await;
    alice.save("feature-2", "two.txt", &f1).await;
    let two = alice.open_pull("Two", "feature-2", "feature-1").await;
    let (status, joined) = join(&alice, number_of(&two)).await;
    assert_eq!(status, StatusCode::OK, "{joined}");

    // A stacked pull request cannot change its base.
    let (status, value) = alice
        .api(
            "PUT",
            &format!("/api/repos/alice/proj/pulls/{}", number_of(&two)),
            json!({"expected_state": "open", "expected_updated": joined["updated_at"], "base": "develop"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert_eq!(
        value["message"],
        "Unstack the pull request before changing its base branch."
    );

    // A pull request whose head is in a fork never joins.
    let fork = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/forks",
            Value::Null,
            StatusCode::CREATED,
        )
        .await;
    let fork_id = fork["repo"]["id"].as_i64().unwrap();
    bob.expect(
        "POST",
        "/api/repos/bob/proj/branches",
        json!({"name": "fork-feature", "sha": main0}),
        StatusCode::OK,
    )
    .await;
    let forked = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls",
            json!({"title": "From fork", "head_branch": "fork-feature", "base_branch": "feature-1", "head_repo_id": fork_id}),
            StatusCode::CREATED,
        )
        .await;
    let (status, value) = join(&alice, number_of(&forked)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert_eq!(
        value["message"],
        "Only a pull request from this repository's own branch can join a stack."
    );

    // A pull request whose base is no open pull request's head has no parent to join.
    alice.branch("solo", &main0).await;
    alice.save("solo", "solo.txt", &main0).await;
    let solo = alice.open_pull("Solo", "solo", "main").await;
    let (status, value) = join(&alice, number_of(&solo)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert_eq!(
        value["message"],
        "No open pull request in this repository has this base branch as its head."
    );

    // An unstacked pull request may change its base, and does not join anything by it.
    let moved = alice
        .expect(
            "PUT",
            &format!("/api/repos/alice/proj/pulls/{}", number_of(&solo)),
            json!({"expected_state": "open", "expected_updated": solo["updated_at"], "base": "develop"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(moved["base_branch"], "develop");
    assert_eq!(moved["stack_id"], Value::Null);

    // A pull request on a layer's branch that has its own layer on top is not the top: refused.
    alice.branch("plain", &f1).await;
    alice.save("plain", "plain.txt", &f1).await;
    let plain = alice.open_pull("Plain", "plain", "feature-1").await;
    let (status, value) = join(&alice, number_of(&plain)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert_eq!(
        value["message"],
        "Something is already stacked on that pull request."
    );

    // Once the bottom layer is closed, Two has no open parent; when a new pull request takes its
    // branch, Two is already in another stack and cannot join it.
    alice
        .expect(
            "PUT",
            &format!("/api/repos/alice/proj/pulls/{}", number_of(&one)),
            json!({"expected_state": "open", "expected_updated": alice.pull(1).await["updated_at"], "state": "closed"}),
            StatusCode::OK,
        )
        .await;
    let (status, value) = join(&alice, number_of(&two)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert_eq!(
        value["message"],
        "No open pull request in this repository has this base branch as its head."
    );
    let replacement = alice.open_pull("Replacement", "feature-1", "main").await;
    let (status, value) = join(&alice, number_of(&two)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert_eq!(
        value["message"],
        "Unstack this pull request before adding it to another stack."
    );
    assert_eq!(replacement["stack_id"], Value::Null);
    f.finish().await;
}

/// Merging the bottom layer moves its stacked layers only: a pull request outside the stack that
/// is based on the same branch, a fork pull request, and a pull request on a merged branch stay
/// where they are. A layer waits for the layers below it, and unstacking keeps the bases.
async fn stack_merge_and_retarget_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    // Trunk moves on, so merging a layer makes a merge commit.
    alice.save("main", "main.txt", &main0).await;

    alice.branch("feature-1", &main0).await;
    let f1 = alice.save("feature-1", "one.txt", &main0).await;
    let one = alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &f1).await;
    let f2 = alice.save("feature-2", "two.txt", &f1).await;
    let two = alice.open_pull("Two", "feature-2", "feature-1").await;
    alice.branch("feature-3", &f2).await;
    alice.save("feature-3", "three.txt", &f2).await;
    alice.open_pull("Three", "feature-3", "feature-2").await;
    let stack_id = one["id"].as_i64().unwrap();
    assert_eq!(number_of(&two), 2);
    for number in [2, 3] {
        let (status, value) = join(&alice, number).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["stack_id"], stack_id);
    }

    // Pull requests that target a layer's branch but are not stacked.
    alice.branch("plain", &f1).await;
    alice.save("plain", "plain.txt", &f1).await;
    let plain = alice.open_pull("Plain", "plain", "feature-1").await;
    assert_eq!(plain["stack_id"], Value::Null);
    let fork = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/forks",
            Value::Null,
            StatusCode::CREATED,
        )
        .await;
    let fork_id = fork["repo"]["id"].as_i64().unwrap();
    bob.expect(
        "POST",
        "/api/repos/bob/proj/branches",
        json!({"name": "fork-feature", "sha": main0}),
        StatusCode::OK,
    )
    .await;
    let forked = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls",
            json!({"title": "From fork", "head_branch": "fork-feature", "base_branch": "feature-1", "head_repo_id": fork_id}),
            StatusCode::CREATED,
        )
        .await;

    // A develop-style pull request merges into trunk; a plain pull request still targets develop.
    alice.branch("develop", &main0).await;
    alice.save("develop", "develop.txt", &main0).await;
    let develop = alice.open_pull("Develop", "develop", "main").await;
    alice.branch("feature-x", &main0).await;
    alice.save("feature-x", "x.txt", &main0).await;
    let on_develop = alice.open_pull("On develop", "feature-x", "develop").await;
    alice.merge_current(number_of(&develop)).await;
    assert_eq!(
        alice.pull(number_of(&on_develop)).await["base_branch"],
        "develop"
    );

    // Merging the bottom layer moves its stacked child onto trunk, and only that child.
    let merged = alice.merge_current(number_of(&one)).await;
    let merge_sha = merged["sha"].clone();
    let two_now = alice.pull(2).await;
    assert_eq!(two_now["base_branch"], "main");
    assert_eq!(two_now["base_sha"], merge_sha);
    assert_eq!(alice.pull(3).await["base_branch"], "feature-2");
    assert_eq!(
        alice.pull(number_of(&plain)).await["base_branch"],
        "feature-1"
    );
    assert_eq!(
        alice.pull(number_of(&forked)).await["base_branch"],
        "feature-1"
    );
    assert_eq!(alice.pull(1).await["stack_id"], stack_id);

    // Asking again to move the same members finds nothing and fails nothing.
    let repository = db::repositories::records::get_repository(f.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    for _ in 0..2 {
        let (moved, left) = db::pull_requests::records::retarget_stacked_pull_requests(
            f.pool(),
            alice_id,
            repository.id,
            stack_id,
            "feature-1",
            "main",
            merge_sha.as_str().unwrap(),
        )
        .await
        .unwrap();
        assert!(moved.is_empty());
        assert!(left.is_empty());
    }

    // The next layer merges, and its child follows it onto trunk.
    let merged_two = alice.merge_current(2).await;
    assert_eq!(alice.pull(3).await["base_branch"], "main");
    assert_eq!(alice.pull(3).await["base_sha"], merged_two["sha"]);

    // Unstacking takes the open layers out and keeps their bases; merged layers stay stacked.
    let unstacked = alice
        .expect(
            "DELETE",
            "/api/repos/alice/proj/pulls/3/stack",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(unstacked["stack_id"], Value::Null);
    assert_eq!(alice.pull(3).await["base_branch"], "main");
    assert_eq!(alice.pull(1).await["stack_id"], stack_id);
    assert_eq!(alice.pull(2).await["stack_id"], stack_id);
    alice
        .expect(
            "DELETE",
            "/api/repos/alice/proj/pulls/3/stack",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let (status, _) = bob
        .api("DELETE", "/api/repos/alice/proj/pulls/3/stack", Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A retarget returns the layers it moved as the move left them. A refresh that moves one of
    // them afterwards does not change what was returned.
    alice.branch("feature-4", &main0).await;
    let f4 = alice.save("feature-4", "four.txt", &main0).await;
    let four = alice.open_pull("Four", "feature-4", "main").await;
    alice.branch("feature-5", &f4).await;
    alice.save("feature-5", "five.txt", &f4).await;
    let five = alice.open_pull("Five", "feature-5", "feature-4").await;
    let (status, value) = join(&alice, number_of(&five)).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let main_head = merged_two["sha"].as_str().unwrap();
    let (moved, left) = db::pull_requests::records::retarget_stacked_pull_requests(
        f.pool(),
        alice_id,
        repository.id,
        four["id"].as_i64().unwrap(),
        "feature-4",
        "main",
        main_head,
    )
    .await
    .unwrap();
    assert!(left.is_empty());
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].id, five["id"].as_i64().unwrap());
    assert_eq!(moved[0].base_branch, "main");
    assert_eq!(moved[0].base_sha, main_head);
    let refreshed = db::pull_requests::records::move_open_pull_request_base(
        f.pool(),
        moved[0].id,
        main_head,
        main0.as_str(),
        "main",
        None,
    )
    .await
    .unwrap();
    assert!(refreshed);
    assert_eq!(moved[0].base_sha, main_head);
    let row = db::pull_requests::records::get_pull_request_by_id(f.pool(), moved[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.base_sha, main0);
    f.finish().await;
}

/// A stacked child whose move would duplicate an open pull request stays on its old base, and the
/// merge still succeeds.
async fn stack_collision_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature-4", &main0).await;
    let f4 = alice.save("feature-4", "four.txt", &main0).await;
    let four = alice.open_pull("Four", "feature-4", "main").await;
    alice.branch("feature-5", &f4).await;
    alice.save("feature-5", "five.txt", &f4).await;
    let five = alice.open_pull("Five", "feature-5", "feature-4").await;
    let (status, value) = join(&alice, number_of(&five)).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    // Another open pull request already merges this head into trunk.
    alice.open_pull("Collider", "feature-5", "main").await;

    alice.merge_current(number_of(&four)).await;
    assert_eq!(
        alice.pull(number_of(&five)).await["base_branch"],
        "feature-4"
    );
    assert_eq!(alice.pull(number_of(&five)).await["stack_id"], four["id"]);
    f.finish().await;
}

/// A child prepared for merging while it was not stacked cannot be published once it has joined a
/// stack above its open parent: publishing checks the order again.
async fn stack_publish_order_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let runs = application_with_runs(&mut f, root.path()).await;
    let alice = Client::login(&runs.router, f.pool(), runs.alice).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature-1", &main0).await;
    let f1 = alice.save("feature-1", "one.txt", &main0).await;
    alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &f1).await;
    alice.save("feature-2", "two.txt", &f1).await;
    let two = alice.open_pull("Two", "feature-2", "feature-1").await;

    let prepared = merge::prepare_pull_request_merge(
        f.pool(),
        &runs.ci,
        runs.alice,
        ("alice".to_owned(), "proj".to_owned(), 2),
        merge::MergeRequest {
            expected_head: pulls::text(&two, "head_sha").to_owned(),
            expected_base: pulls::text(&two, "base_sha").to_owned(),
            expected_base_branch: None,
            expected_target: None,
            merge_method: Default::default(),
        },
    )
    .await
    .unwrap();
    let (status, value) = join(&alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let published =
        merge::publish_prepared_merge(f.pool(), &runs.ci, &runs.broadcaster, prepared).await;
    match published {
        Err(OperationError::Conflict { code, .. }) => assert_eq!(code, "revision_conflict"),
        Err(error) => panic!("unexpected refusal: {error:?}"),
        Ok(_) => panic!("a merge prepared before the stack changed was published"),
    }
    assert_eq!(alice.pull(2).await["state"], "open");
    f.finish().await;
}

/// Layers merge from the bottom up, so the map lists merged layers by merge time, not by number:
/// the lower layer here has the higher number and merges first.
async fn stack_merge_order_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature-a", &main0).await;
    let fa = alice.save("feature-a", "a.txt", &main0).await;
    alice.branch("feature-b", &fa).await;
    alice.save("feature-b", "b.txt", &fa).await;
    let upper = alice.open_pull("Upper", "feature-b", "feature-a").await;
    alice.open_pull("Lower", "feature-a", "main").await;
    assert_eq!(number_of(&upper), 1);
    let (status, value) = join(&alice, 1).await;
    assert_eq!(status, StatusCode::OK, "{value}");

    alice.merge_current(2).await;
    alice.merge_current(1).await;
    let pull = alice.pull(1).await;
    let numbers: Vec<i64> = pull["stack"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(number_of)
        .collect();
    assert_eq!(numbers, vec![2, 1]);
    f.finish().await;
}

/// Branches that are based on each other never join a stack. A stack stored with such branches
/// refuses both layers: neither merges.
async fn stack_cycles_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("a", &main0).await;
    alice.save("a", "a.txt", &main0).await;
    alice.branch("b", &main0).await;
    alice.save("b", "b.txt", &main0).await;
    let first = alice.open_pull("First", "a", "b").await;
    let second = alice.open_pull("Second", "b", "a").await;

    for number in [1, 2] {
        let (status, value) = join(&alice, number).await;
        assert_eq!(status, StatusCode::CONFLICT, "{value}");
        assert_eq!(
            value["message"],
            "These pull requests would depend on each other, so they cannot be stacked."
        );
    }
    assert_eq!(alice.pull(1).await["stack_id"], Value::Null);
    assert_eq!(alice.pull(2).await["stack_id"], Value::Null);

    // Both layers stored as one stack are refused as broken.
    let (first_id, second_id) = (
        first["id"].as_i64().unwrap(),
        second["id"].as_i64().unwrap(),
    );
    f.execute(
        &format!(
            "UPDATE pull_requests SET stack_id = {first_id} WHERE id IN ({first_id}, {second_id})"
        ),
        &[],
    )
    .await;
    for number in [1, 2] {
        let pull = alice.pull(number).await;
        let (status, value) = alice
            .api(
                "POST",
                &format!("/api/repos/alice/proj/pulls/{number}/merge"),
                json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"]}),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{value}");
        assert_eq!(value["error"], "stack_broken");
        assert_eq!(alice.pull(number).await["state"], "open");
    }
    f.finish().await;
}

/// Closing the middle layer of a stack leaves the bottom layer mergeable. The layer above the
/// closed one is refused with the closed-below answer, and its base does not move.
async fn stack_closed_middle_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature-1", &main0).await;
    let f1 = alice.save("feature-1", "one.txt", &main0).await;
    alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &f1).await;
    let f2 = alice.save("feature-2", "two.txt", &f1).await;
    alice.open_pull("Two", "feature-2", "feature-1").await;
    alice.branch("feature-3", &f2).await;
    alice.save("feature-3", "three.txt", &f2).await;
    alice.open_pull("Three", "feature-3", "feature-2").await;
    for number in [2, 3] {
        let (status, value) = join(&alice, number).await;
        assert_eq!(status, StatusCode::OK, "{value}");
    }
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/pulls/2",
            json!({"expected_state": "open", "expected_updated": alice.pull(2).await["updated_at"], "state": "closed"}),
            StatusCode::OK,
        )
        .await;

    let top = alice.pull(3).await;
    let (status, value) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/3/merge",
            json!({"expected_head": top["head_sha"], "expected_base": top["base_sha"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert_eq!(value["error"], "stack_layer_closed");
    assert_eq!(
        value["message"],
        "Pull request #2 below it is closed. Reopen it or unstack this pull request."
    );

    // The bottom layer merges as usual, and the closed layer does not move the top one.
    alice.merge_current(1).await;
    assert_eq!(alice.pull(1).await["state"], "merged");
    assert_eq!(alice.pull(3).await["base_branch"], "feature-2");
    f.finish().await;
}

/// A parent whose earlier child has closed takes a new child: a closed layer does not sit on top.
async fn stack_join_after_closed_child_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature-1", &main0).await;
    let f1 = alice.save("feature-1", "one.txt", &main0).await;
    let one = alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &f1).await;
    alice.save("feature-2", "two.txt", &f1).await;
    alice.open_pull("Two", "feature-2", "feature-1").await;
    let (status, value) = join(&alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/pulls/2",
            json!({"expected_state": "open", "expected_updated": alice.pull(2).await["updated_at"], "state": "closed"}),
            StatusCode::OK,
        )
        .await;

    alice.branch("feature-3", &f1).await;
    alice.save("feature-3", "three.txt", &f1).await;
    let three = alice.open_pull("Three", "feature-3", "feature-1").await;
    let (status, value) = join(&alice, number_of(&three)).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["stack_id"], one["id"]);
    f.finish().await;
}

/// Posts the page's merge form for pull request `number` with the values the page shows.
async fn merge_form(alice: &Client, number: i64) -> (StatusCode, String) {
    let pull = alice.pull(number).await;
    alice
        .form(
            &format!("/alice/proj/pulls/{number}/merge"),
            &[
                ("expected_state", pulls::text(&pull, "state")),
                ("expected_updated", pulls::text(&pull, "updated_at")),
                ("expected_head", pulls::text(&pull, "head_sha")),
                ("expected_base", pulls::text(&pull, "base_sha")),
                ("confirm", "merge"),
            ],
        )
        .await
}

/// The page's merge and stack forms say why they refused: a merge above an open layer, a merge
/// above a closed one, and a join that has no open parent.
async fn stack_native_refusals_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature-1", &main0).await;
    let f1 = alice.save("feature-1", "one.txt", &main0).await;
    alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &f1).await;
    alice.save("feature-2", "two.txt", &f1).await;
    alice.open_pull("Two", "feature-2", "feature-1").await;
    let (status, value) = join(&alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{value}");

    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/pulls/1",
            json!({"expected_state": "open", "expected_updated": alice.pull(1).await["updated_at"], "state": "closed"}),
            StatusCode::OK,
        )
        .await;
    let (status, page) = merge_form(&alice, 2).await;
    assert_eq!(status, StatusCode::CONFLICT, "{page}");
    assert!(
        page.contains(
            "Pull request #1 below it is closed. Reopen it or unstack this pull request."
        ),
        "{page}"
    );

    alice.branch("solo", &main0).await;
    alice.save("solo", "solo.txt", &main0).await;
    let solo = alice.open_pull("Solo", "solo", "main").await;
    let (status, page) = alice
        .form(
            &format!("/alice/proj/pulls/{}/stack", number_of(&solo)),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{page}");
    assert!(
        page.contains("No open pull request in this repository has this base branch as its head."),
        "{page}"
    );
    f.finish().await;
}

/// The loop is looked for among the members a join would form, so an unrelated open pull request
/// that shares a head branch with one of them cannot hide it.
async fn stack_cycle_beside_shared_head_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("a", &main0).await;
    alice.save("a", "a.txt", &main0).await;
    alice.branch("b", &main0).await;
    alice.save("b", "b.txt", &main0).await;
    // One and Three both have head a; Two is based on a and Three on b, so Two and Three loop.
    alice.open_pull("One", "a", "main").await;
    alice.open_pull("Two", "b", "a").await;
    alice.open_pull("Three", "a", "b").await;

    let (status, value) = join(&alice, 3).await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert_eq!(
        value["message"],
        "These pull requests would depend on each other, so they cannot be stacked."
    );
    // Two may join One, since that is no loop. With that stack in place, Three is still refused.
    let (status, value) = join(&alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let (status, value) = join(&alice, 3).await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert_eq!(
        value["message"],
        "These pull requests would depend on each other, so they cannot be stacked."
    );
    assert_eq!(alice.pull(3).await["stack_id"], Value::Null);
    f.finish().await;
}

/// A join takes the next open parent when the lowest-numbered one whose head is its base branch
/// has merged.
async fn stack_join_replacement_parent_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("develop", &main0).await;
    alice.branch("shared", &main0).await;
    let shared = alice.save("shared", "shared.txt", &main0).await;
    let first = alice.open_pull("First", "shared", "main").await;
    let second = alice.open_pull("Second", "shared", "develop").await;
    alice.branch("child", &shared).await;
    alice.save("child", "child.txt", &shared).await;
    let child = alice.open_pull("Child", "child", "shared").await;
    f.execute(
        &format!(
            "UPDATE pull_requests SET state = 'merged', merged = TRUE WHERE id = {}",
            first["id"].as_i64().unwrap()
        ),
        &[],
    )
    .await;

    let (status, value) = join(&alice, number_of(&child)).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["stack_id"], second["id"]);
    assert_eq!(
        alice.pull(number_of(&second)).await["stack_id"],
        second["id"]
    );
    assert_eq!(alice.pull(number_of(&first)).await["stack_id"], Value::Null);
    f.finish().await;
}

/// A delayed unstack names the pull request it was asked about, not the stack it saw. When that
/// pull request is no longer stacked and a new stack reuses the id, repeating the unstack must
/// leave the new stack alone.
async fn stack_unstack_by_target_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature-1", &main0).await;
    let f1 = alice.save("feature-1", "one.txt", &main0).await;
    let one = alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &f1).await;
    alice.save("feature-2", "two.txt", &f1).await;
    let two = alice.open_pull("Two", "feature-2", "feature-1").await;
    alice.branch("feature-3", &f1).await;
    alice.save("feature-3", "three.txt", &f1).await;
    let three = alice.open_pull("Three", "feature-3", "feature-1").await;
    let (one_id, two_id, three_id) = (
        one["id"].as_i64().unwrap(),
        two["id"].as_i64().unwrap(),
        three["id"].as_i64().unwrap(),
    );
    let (status, value) = join(&alice, number_of(&two)).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["stack_id"], one_id);

    // The first unstack takes the stack apart. Another pull request then starts a new stack on the
    // same bottom layer, which gets the same stack id.
    assert!(
        db::pull_requests::records::unstack_pull_requests(f.pool(), alice_id, two_id)
            .await
            .unwrap()
    );
    let joined = db::pull_requests::records::join_pull_request_stack(f.pool(), alice_id, three_id)
        .await
        .unwrap();
    assert_eq!(joined, StackJoin::Joined(one_id));

    // Repeating the unstack by its target: it is in no stack, so nothing changes and it succeeds.
    assert!(
        db::pull_requests::records::unstack_pull_requests(f.pool(), alice_id, two_id)
            .await
            .unwrap()
    );
    for (id, stack_id) in [
        (one_id, Some(one_id)),
        (three_id, Some(one_id)),
        (two_id, None),
    ] {
        let row = db::pull_requests::records::get_pull_request_by_id(f.pool(), id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.stack_id, stack_id, "pull request {id}");
    }
    f.finish().await;
}

/// Waits until a backend is blocked by a lock that backend `pid` holds.
async fn wait_until_blocked_by(pool: &sqlx::PgPool, pid: i32) {
    for _ in 0..1000 {
        let blocked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock' AND $1 = ANY(pg_blocking_pids(pid))",
        )
        .bind(pid)
        .fetch_one(pool)
        .await
        .unwrap();
        if blocked > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no backend waited for the merge");
}

/// A stack can only be merged with merge commits, so a branch that requires a linear history
/// leaves no method to choose. The page says the merge is blocked instead of offering it.
async fn stack_on_a_linear_history_branch_is_blocked_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature-1", &main0).await;
    let f1 = alice.save("feature-1", "one.txt", &main0).await;
    alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &f1).await;
    alice.save("feature-2", "two.txt", &f1).await;
    alice.open_pull("Two", "feature-2", "feature-1").await;
    let (status, joined) = join(&alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({"pattern": "main", "require_pull_request": true, "require_linear_history": true}),
            StatusCode::OK,
        )
        .await;

    let (status, page) = alice.page("/alice/proj/pulls/2").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Merging is blocked"), "{page}");
    assert!(page.contains("Merge commits are not allowed"), "{page}");
    assert!(!page.contains("Ready to merge"), "{page}");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_stack_on_a_linear_history_branch_is_blocked() {
    stack_on_a_linear_history_branch_is_blocked_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_on_a_linear_history_branch_is_blocked() {
    stack_on_a_linear_history_branch_is_blocked_contract(true).await;
}

#[tokio::test]
async fn sqlite_stacks_join_and_order() {
    stacks_join_and_order_contract(false).await;
}

#[tokio::test]
async fn postgres_stacks_join_and_order() {
    stacks_join_and_order_contract(true).await;
}

#[tokio::test]
async fn sqlite_stacks_join_refusals() {
    stacks_join_refusals_contract(false).await;
}

#[tokio::test]
async fn postgres_stacks_join_refusals() {
    stacks_join_refusals_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_merge_and_retarget() {
    stack_merge_and_retarget_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_merge_and_retarget() {
    stack_merge_and_retarget_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_collision_stays_put() {
    stack_collision_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_collision_stays_put() {
    stack_collision_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_publish_checks_order() {
    stack_publish_order_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_publish_checks_order() {
    stack_publish_order_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_merge_order_follows_merges() {
    stack_merge_order_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_merge_order_follows_merges() {
    stack_merge_order_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_cycles_are_refused() {
    stack_cycles_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_cycles_are_refused() {
    stack_cycles_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_join_after_closed_child() {
    stack_join_after_closed_child_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_join_after_closed_child() {
    stack_join_after_closed_child_contract(true).await;
}

/// On PostgreSQL, a join reads its parent as open and then waits for the parent's row. A merge
/// that marks the parent commits first, so the join sees the parent merged and is refused: the
/// child is not stacked onto a merged parent. SQLite serializes both writes with BEGIN IMMEDIATE.
#[tokio::test]
async fn postgres_join_waits_for_the_merge_that_marks_its_parent() {
    let mut f = Database::new(true).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature-1", &main0).await;
    let f1 = alice.save("feature-1", "one.txt", &main0).await;
    let one = alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &f1).await;
    alice.save("feature-2", "two.txt", &f1).await;
    let two = alice.open_pull("Two", "feature-2", "feature-1").await;
    let (one_id, two_id) = (one["id"].as_i64().unwrap(), two["id"].as_i64().unwrap());

    let db::DbPool::Postgres(raw) = f.pool().clone() else {
        panic!("this test needs the PostgreSQL fixture");
    };
    // The merge's marking of the parent, in a transaction that stays open for now.
    let mut merging = raw.begin().await.unwrap();
    let merging_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *merging)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE pull_requests SET state = 'merged', merged = TRUE, merged_at = NOW(), closed_at = NOW(), updated_at = NOW() WHERE id = $1",
    )
    .bind(one_id)
    .execute(&mut *merging)
    .await
    .unwrap();

    // The join reads the parent as open, then waits for the row the merge holds.
    let pool = f.pool().clone();
    let joining = tokio::spawn(async move {
        db::pull_requests::records::join_pull_request_stack(&pool, alice_id, two_id).await
    });
    wait_until_blocked_by(&raw, merging_pid).await;
    merging.commit().await.unwrap();
    let answer = joining.await.unwrap().unwrap();
    assert_eq!(answer, StackJoin::Refused(StackRefusal::NoParent));
    let child = db::pull_requests::records::get_pull_request_by_id(f.pool(), two_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.stack_id, None);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_stack_closed_middle_layer() {
    stack_closed_middle_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_closed_middle_layer() {
    stack_closed_middle_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_native_refusals_name_the_reason() {
    stack_native_refusals_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_native_refusals_name_the_reason() {
    stack_native_refusals_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_cycle_beside_shared_head_is_refused() {
    stack_cycle_beside_shared_head_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_cycle_beside_shared_head_is_refused() {
    stack_cycle_beside_shared_head_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_join_takes_the_replacement_parent() {
    stack_join_replacement_parent_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_join_takes_the_replacement_parent() {
    stack_join_replacement_parent_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_unstack_by_target_is_idempotent() {
    stack_unstack_by_target_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_unstack_by_target_is_idempotent() {
    stack_unstack_by_target_contract(true).await;
}

/// On PostgreSQL a join decides on the first open parent it reads and waits for that row. When that
/// parent merges meanwhile, the join decides again on the next open parent, under its own lock. When
/// that one merges too while the join waits, nothing is joined.
#[tokio::test]
async fn postgres_join_refuses_a_replacement_parent_merged_meanwhile() {
    let mut f = Database::new(true).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("develop", &main0).await;
    alice.branch("shared", &main0).await;
    let shared = alice.save("shared", "shared.txt", &main0).await;
    let first = alice.open_pull("First", "shared", "main").await;
    let second = alice.open_pull("Second", "shared", "develop").await;
    alice.branch("child", &shared).await;
    alice.save("child", "child.txt", &shared).await;
    let child = alice.open_pull("Child", "child", "shared").await;
    let (first_id, second_id, child_id) = (
        first["id"].as_i64().unwrap(),
        second["id"].as_i64().unwrap(),
        child["id"].as_i64().unwrap(),
    );

    let db::DbPool::Postgres(raw) = f.pool().clone() else {
        panic!("this test needs the PostgreSQL fixture");
    };
    // Both parents are marked merged, each by a transaction that stays open for now.
    let mut merging_first = raw.begin().await.unwrap();
    let first_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *merging_first)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE pull_requests SET state = 'merged', merged = TRUE, merged_at = NOW(), closed_at = NOW(), updated_at = NOW() WHERE id = $1",
    )
    .bind(first_id)
    .execute(&mut *merging_first)
    .await
    .unwrap();
    let mut merging_second = raw.begin().await.unwrap();
    let second_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *merging_second)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE pull_requests SET state = 'merged', merged = TRUE, merged_at = NOW(), closed_at = NOW(), updated_at = NOW() WHERE id = $1",
    )
    .bind(second_id)
    .execute(&mut *merging_second)
    .await
    .unwrap();

    let pool = f.pool().clone();
    let joining = tokio::spawn(async move {
        db::pull_requests::records::join_pull_request_stack(&pool, alice_id, child_id).await
    });
    // The join decides on the first parent and waits for its row.
    wait_until_blocked_by(&raw, first_pid).await;
    merging_first.commit().await.unwrap();
    // It decides again on the second parent, locks that row, and waits for it.
    wait_until_blocked_by(&raw, second_pid).await;
    merging_second.commit().await.unwrap();

    let answer = joining.await.unwrap().unwrap();
    assert_eq!(answer, StackJoin::Refused(StackRefusal::NoParent));
    for id in [first_id, second_id, child_id] {
        let row = db::pull_requests::records::get_pull_request_by_id(f.pool(), id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.stack_id, None, "pull request {id} must not be stacked");
    }
    f.finish().await;
}

/// The repository `proj` of alice.
async fn proj(f: &Database) -> db::models::Repository {
    db::repositories::records::get_repository(f.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap()
}

/// The commit that `main` points at in the bare repository at `path`.
fn main_tip(path: &str) -> String {
    git2::Repository::open_bare(path)
        .unwrap()
        .refname_to_id("refs/heads/main")
        .unwrap()
        .to_string()
}

/// The merge commit that the database recorded for pull request `number`.
async fn merge_commit_of(f: &Database, number: i32) -> Option<String> {
    let repository = proj(f).await;
    db::pull_requests::records::get_pull_request(f.pool(), repository.id, number)
        .await
        .unwrap()
        .unwrap()
        .merge_commit_sha
}

/// Three layers, each based on the one below. With `trunk_moves`, trunk moves on after the stack
/// starts; otherwise trunk is still where the stack started, so every layer already contains it.
/// Layers 2 and 3 are joined to the stack. Returns the tips of the three layer branches.
async fn three_layer_stack(alice: &Client, trunk_moves: bool) -> (String, String, String) {
    let main0 = alice.save("main", "README.md", "unborn").await;
    if trunk_moves {
        alice.save("main", "main.txt", &main0).await;
    }
    alice.branch("feature-1", &main0).await;
    let one = alice.save("feature-1", "one.txt", &main0).await;
    alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &one).await;
    let two = alice.save("feature-2", "two.txt", &one).await;
    alice.open_pull("Two", "feature-2", "feature-1").await;
    alice.branch("feature-3", &two).await;
    let three = alice.save("feature-3", "three.txt", &two).await;
    alice.open_pull("Three", "feature-3", "feature-2").await;
    for number in [2, 3] {
        let (status, value) = join(alice, number).await;
        assert_eq!(status, StatusCode::OK, "{value}");
    }
    (one, two, three)
}

/// Prepares the merge of pull request `number` at the revisions it shows now, as the merge API does.
async fn prepare_merge(
    f: &Database,
    runs: &Runs,
    alice: &Client,
    number: i64,
) -> merge::PreparedMerge {
    let pull = alice.pull(number).await;
    merge::prepare_pull_request_merge(
        f.pool(),
        &runs.ci,
        runs.alice,
        ("alice".to_owned(), "proj".to_owned(), number as i32),
        merge::MergeRequest {
            expected_head: pulls::text(&pull, "head_sha").to_owned(),
            expected_base: pulls::text(&pull, "base_sha").to_owned(),
            expected_base_branch: None,
            expected_target: None,
            merge_method: Default::default(),
        },
    )
    .await
    .unwrap()
}

/// The top layer merges the whole stack: one trunk move to a merge commit that holds the top layer,
/// and every layer is marked merged with that commit.
async fn stack_chain_merge_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let path = proj(&f).await.disk_path;
    // Trunk did not move since the stack started, so every layer already contains trunk. The
    // merge still makes a merge commit of trunk and the top layer, as a single merge does.
    let (_, _, three) = three_layer_stack(&alice, false).await;
    let trunk = main_tip(&path);

    let merged = alice.merge_current(3).await;
    let sha = merged["sha"].as_str().unwrap().to_owned();
    assert_eq!(main_tip(&path), sha);
    let git = git2::Repository::open_bare(&path).unwrap();
    let commit = git.find_commit(git2::Oid::from_str(&sha).unwrap()).unwrap();
    assert_eq!(
        commit.parent_ids().collect::<Vec<_>>(),
        vec![
            git2::Oid::from_str(&trunk).unwrap(),
            git2::Oid::from_str(&three).unwrap()
        ]
    );
    assert_eq!(
        commit.tree_id(),
        git.find_commit(git2::Oid::from_str(&three).unwrap())
            .unwrap()
            .tree_id()
    );
    for number in [1, 2, 3] {
        assert_eq!(alice.pull(number).await["state"], "merged");
        assert_eq!(
            merge_commit_of(&f, number as i32).await.as_deref(),
            Some(sha.as_str())
        );
    }
    f.finish().await;
}

/// A middle layer merges the layers below it. The layer above it moves onto trunk and stays open.
async fn stack_middle_merge_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let path = proj(&f).await.disk_path;
    three_layer_stack(&alice, true).await;

    let merged = alice.merge_current(2).await;
    let sha = merged["sha"].as_str().unwrap().to_owned();
    assert_eq!(main_tip(&path), sha);
    // Trunk moved on, so the merge is a commit of its own that brings in the trunk and the layers.
    let git = git2::Repository::open_bare(&path).unwrap();
    let commit = git.find_commit(git2::Oid::from_str(&sha).unwrap()).unwrap();
    assert_eq!(commit.parent_count(), 2);
    let tree = commit.tree().unwrap();
    for file in ["main.txt", "one.txt", "two.txt"] {
        assert!(
            tree.get_name(file).is_some(),
            "{file} is missing from the merge"
        );
    }
    for number in [1, 2] {
        assert_eq!(alice.pull(number).await["state"], "merged");
        assert_eq!(
            merge_commit_of(&f, number as i32).await.as_deref(),
            Some(sha.as_str())
        );
    }
    let top = alice.pull(3).await;
    assert_eq!(top["state"], "open");
    assert_eq!(top["base_branch"], "main");
    assert_eq!(top["base_sha"], sha);

    alice.merge_current(3).await;
    assert_eq!(alice.pull(3).await["state"], "merged");
    f.finish().await;
}

/// Protection applies to every layer of the stack. The bottom layer has no approval, so the merge
/// is refused and names it. Approving the layers in turn lets the merge through.
async fn stack_policy_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let runs = application_with_runs(&mut f, root.path()).await;
    let alice = Client::login(&runs.router, f.pool(), runs.alice).await;
    let bob = Client::login(&runs.router, f.pool(), runs.bob).await;
    db::repositories::access::grant_repo_access(
        f.pool(),
        runs.bob,
        proj(&f).await.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let (one, two, three) = three_layer_stack(&alice, true).await;
    db::repositories::protection::upsert_branch_protection_rule(
        f.pool(),
        db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: runs.alice,
            repo_id: proj(&f).await.id,
            pattern: "main",
            require_pull_request: false,
            required_approving_reviews: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let path = proj(&f).await.disk_path;
    let trunk = main_tip(&path);

    let (status, refused) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/3/merge",
            json!({"expected_head": three, "expected_base": alice.pull(3).await["base_sha"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "branch_protection", "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .starts_with("Pull request #1: "),
        "{refused}"
    );
    assert_eq!(main_tip(&path), trunk);
    for number in [1, 2, 3] {
        assert_eq!(alice.pull(number).await["state"], "open");
    }

    for (number, head) in [(3, &three), (2, &two), (1, &one)] {
        let (status, value) = bob
            .api(
                "POST",
                &format!("/api/repos/alice/proj/pulls/{number}/reviews"),
                json!({"state": "approved", "commit_sha": head}),
            )
            .await;
        assert!(status.is_success(), "{value}");
    }
    alice.merge_current(3).await;
    for number in [1, 2, 3] {
        assert_eq!(alice.pull(number).await["state"], "merged");
    }
    f.finish().await;
}

/// A draft layer below the top one refuses the merge, and the refusal names it.
async fn stack_draft_below_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature-1", &main0).await;
    let one = alice.save("feature-1", "one.txt", &main0).await;
    alice.open_draft("One", "feature-1", "main").await;
    alice.branch("feature-2", &one).await;
    alice.save("feature-2", "two.txt", &one).await;
    alice.open_pull("Two", "feature-2", "feature-1").await;
    let (status, value) = join(&alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{value}");

    let top = alice.pull(2).await;
    let (status, refused) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/2/merge",
            json!({"expected_head": top["head_sha"], "expected_base": top["base_sha"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "unsupported_merge", "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .starts_with("Pull request #1 "),
        "{refused}"
    );
    assert_eq!(alice.pull(1).await["state"], "open");
    assert_eq!(alice.pull(2).await["state"], "open");
    f.finish().await;
}

/// A layer whose branch moved after it was stacked no longer contains the head of the layer below
/// it. The merge asks for a restack and marks nothing.
async fn stack_restack_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let path = proj(&f).await.disk_path;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature-1", &main0).await;
    let one = alice.save("feature-1", "one.txt", &main0).await;
    alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &one).await;
    alice.save("feature-2", "two.txt", &one).await;
    alice.open_pull("Two", "feature-2", "feature-1").await;
    let (status, value) = join(&alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    alice.save("feature-1", "again.txt", &one).await;

    let top = alice.pull(2).await;
    let (status, refused) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/2/merge",
            json!({"expected_head": top["head_sha"], "expected_base": top["base_sha"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "stack_needs_restack", "{refused}");
    assert_eq!(
        refused["message"],
        "Rebase the stack first: press Rebase stack in the stack panel."
    );
    assert_eq!(main_tip(&path), main0);
    assert_eq!(alice.pull(1).await["state"], "open");
    assert_eq!(alice.pull(2).await["state"], "open");
    f.finish().await;
}

/// A merge prepared for a stack is refused when any layer or the trunk moves before it is
/// published, and nothing is marked.
async fn stack_moved_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let runs = application_with_runs(&mut f, root.path()).await;
    let alice = Client::login(&runs.router, f.pool(), runs.alice).await;
    let path = proj(&f).await.disk_path;
    let (_, _, three) = three_layer_stack(&alice, true).await;
    let trunk = main_tip(&path);

    let prepared = prepare_merge(&f, &runs, &alice, 3).await;
    alice.save("feature-3", "again.txt", &three).await;
    let moved =
        merge::publish_prepared_merge(f.pool(), &runs.ci, &runs.broadcaster, prepared).await;
    assert!(
        matches!(
            moved,
            Err(OperationError::Conflict {
                code: "revision_conflict",
                ..
            })
        ),
        "a layer moved after the merge was prepared and the merge was published"
    );
    assert_eq!(main_tip(&path), trunk);
    for number in [1, 2, 3] {
        assert_eq!(alice.pull(number).await["state"], "open");
    }

    let prepared = prepare_merge(&f, &runs, &alice, 3).await;
    alice.save("main", "later.txt", &trunk).await;
    let moved =
        merge::publish_prepared_merge(f.pool(), &runs.ci, &runs.broadcaster, prepared).await;
    assert!(
        matches!(
            moved,
            Err(OperationError::Conflict {
                code: "revision_conflict",
                ..
            })
        ),
        "the trunk moved after the merge was prepared and the merge was published"
    );
    for number in [1, 2, 3] {
        assert_eq!(alice.pull(number).await["state"], "open");
    }
    f.finish().await;
}

/// Two merges of one stack prepared together: one publish lands the stack and the other is refused
/// as a conflict. The stack is marked once, with one merge commit.
async fn stack_double_merge_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let runs = application_with_runs(&mut f, root.path()).await;
    let alice = Client::login(&runs.router, f.pool(), runs.alice).await;
    let path = proj(&f).await.disk_path;
    three_layer_stack(&alice, true).await;

    let first = prepare_merge(&f, &runs, &alice, 3).await;
    let second = prepare_merge(&f, &runs, &alice, 3).await;
    let (first, second) = tokio::join!(
        merge::publish_prepared_merge(f.pool(), &runs.ci, &runs.broadcaster, first),
        merge::publish_prepared_merge(f.pool(), &runs.ci, &runs.broadcaster, second),
    );
    let (winner, loser) = match (first, second) {
        (Ok(won), Err(lost)) | (Err(lost), Ok(won)) => (won, lost),
        _ => panic!("exactly one of two merges of one stack must win"),
    };
    assert!(
        matches!(
            loser,
            OperationError::Conflict {
                code: "revision_conflict",
                ..
            }
        ),
        "{loser:?}"
    );
    assert_eq!(main_tip(&path), winner.sha);
    for number in [1, 2, 3] {
        assert_eq!(
            merge_commit_of(&f, number).await.as_deref(),
            Some(winner.sha.as_str())
        );
    }
    f.finish().await;
}

#[tokio::test]
async fn sqlite_stack_chain_merge_lands_every_layer() {
    stack_chain_merge_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_chain_merge_lands_every_layer() {
    stack_chain_merge_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_middle_merge_retargets_the_top() {
    stack_middle_merge_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_middle_merge_retargets_the_top() {
    stack_middle_merge_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_policy_names_the_layer_without_approval() {
    stack_policy_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_policy_names_the_layer_without_approval() {
    stack_policy_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_draft_below_is_refused_and_named() {
    stack_draft_below_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_draft_below_is_refused_and_named() {
    stack_draft_below_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_stale_layer_asks_for_a_restack() {
    stack_restack_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_stale_layer_asks_for_a_restack() {
    stack_restack_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_publish_refuses_a_moved_layer_or_trunk() {
    stack_moved_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_publish_refuses_a_moved_layer_or_trunk() {
    stack_moved_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_double_merge_has_one_winner() {
    stack_double_merge_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_double_merge_has_one_winner() {
    stack_double_merge_contract(true).await;
}

/// A child closes, a sibling stacks on the same parent, and then the child reopens. Merging the
/// sibling merges the parent with it, and the reopened child follows them onto trunk.
async fn stack_sibling_merge_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let path = proj(&f).await.disk_path;
    let main0 = alice.save("main", "README.md", "unborn").await;
    // Trunk moves on, so the merge is a merge commit.
    alice.save("main", "main.txt", &main0).await;
    alice.branch("feature-1", &main0).await;
    let one = alice.save("feature-1", "one.txt", &main0).await;
    alice.open_pull("Parent", "feature-1", "main").await;
    alice.branch("feature-2", &one).await;
    alice.save("feature-2", "two.txt", &one).await;
    alice.open_pull("Child", "feature-2", "feature-1").await;
    alice.branch("feature-3", &one).await;
    alice.save("feature-3", "three.txt", &one).await;
    alice.open_pull("Sibling", "feature-3", "feature-1").await;

    let (status, value) = join(&alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/pulls/2",
            json!({"expected_state": "open", "expected_updated": alice.pull(2).await["updated_at"], "state": "closed"}),
            StatusCode::OK,
        )
        .await;
    let (status, value) = join(&alice, 3).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/pulls/2",
            json!({"expected_state": "closed", "expected_updated": alice.pull(2).await["updated_at"], "state": "open"}),
            StatusCode::OK,
        )
        .await;

    let merged = alice.merge_current(3).await;
    let sha = merged["sha"].as_str().unwrap().to_owned();
    assert_eq!(main_tip(&path), sha);
    for number in [1, 3] {
        assert_eq!(alice.pull(number).await["state"], "merged");
    }
    let child = alice.pull(2).await;
    assert_eq!(child["state"], "open");
    assert_eq!(child["base_branch"], "main");
    assert_eq!(child["base_sha"], sha);
    f.finish().await;
}

/// Saves `content` as the CI workflow of `main` and returns the new tip.
async fn save_workflow(client: &Client, tip: &str, content: &str) -> String {
    use base64::Engine as _;
    let saved = client
        .expect(
            "POST",
            "/api/repos/alice/proj/contents/.github/workflows/ci.yml",
            json!({"branch": "main", "expected_revision": tip, "message": "Add workflow",
                "content": base64::engine::general_purpose::STANDARD.encode(content)}),
            StatusCode::CREATED,
        )
        .await;
    saved["sha"].as_str().unwrap().to_owned()
}

/// A workflow that applies to `main` only. A layer of a stack runs it for the trunk its stack
/// merges into, so joining the layer and then pushing a new head both queue its run, and the run
/// is recorded against the layer with a pending commit status.
async fn stack_upper_layer_ci_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let runs = application_with_runs(&mut f, root.path()).await;
    // Only administrators and trusted accounts may have workflows discovered.
    f.execute(
        "UPDATE users SET is_admin = TRUE WHERE id = $1",
        &[runs.alice],
    )
    .await;
    let alice = Client::login(&runs.router, f.pool(), runs.alice).await;
    let repository = proj(&f).await;
    let main0 = alice.save("main", "README.md", "unborn").await;
    let main1 = save_workflow(
        &alice,
        &main0,
        "on:\n  pull_request:\n    branches: [main]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
    )
    .await;
    alice.branch("feature-1", &main1).await;
    let one = alice.save("feature-1", "one.txt", &main1).await;
    alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &one).await;
    let two = alice.save("feature-2", "two.txt", &one).await;
    let upper = alice.open_pull("Two", "feature-2", "feature-1").await;
    let (status, value) = join(&alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{value}");

    let joined = db::ci::latest_ci_runs_for_commit(
        f.pool(),
        repository.id,
        &two,
        upper["id"].as_i64().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(joined.len(), 1, "joining the layer queues its workflow");
    assert_eq!(joined[0].pull_request_id, upper["id"].as_i64());
    assert_eq!(joined[0].branch, "main");
    assert_eq!(joined[0].status, "queued");

    let pushed = alice.save("feature-2", "again.txt", &two).await;
    let queued = db::ci::latest_ci_runs_for_commit(
        f.pool(),
        repository.id,
        &pushed,
        upper["id"].as_i64().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(queued.len(), 1, "a new head queues the layer's workflow");
    assert_eq!(queued[0].pull_request_id, upper["id"].as_i64());
    assert_eq!(queued[0].branch, "main");
    assert_eq!(queued[0].status, "queued");
    let statuses =
        db::repositories::protection::list_commit_statuses(f.pool(), repository.id, &pushed)
            .await
            .unwrap();
    assert!(
        statuses.iter().any(|status| status.state == "pending"),
        "{statuses:?}"
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_stack_sibling_merge_retargets_the_reopened_child() {
    stack_sibling_merge_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_sibling_merge_retargets_the_reopened_child() {
    stack_sibling_merge_contract(true).await;
}

#[tokio::test]
async fn sqlite_stack_upper_layer_runs_the_trunk_workflows() {
    stack_upper_layer_ci_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_upper_layer_runs_the_trunk_workflows() {
    stack_upper_layer_ci_contract(true).await;
}

/// The merge form of a stacked layer confirms the branch it lands in. The layer is unstacked in
/// another session before the form is submitted, and no branch moves, so the layer would land on its
/// own base instead. Both the form and the API refuse that merge, and nothing is merged. A merge that
/// names the branch the layer now lands in still goes through.
async fn stack_stale_target_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let (router, alice_id, _bob_id) = application(&mut f, root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let path = proj(&f).await.disk_path;
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature-1", &main0).await;
    let one = alice.save("feature-1", "one.txt", &main0).await;
    alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &one).await;
    alice.save("feature-2", "two.txt", &one).await;
    alice.open_pull("Two", "feature-2", "feature-1").await;
    let (status, value) = join(&alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    // The page was read while the layer was stacked on the bottom layer, so it confirms main.
    let (status, value) = alice
        .api("DELETE", "/api/repos/alice/proj/pulls/2/stack", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{value}");

    let pull = alice.pull(2).await;
    assert_eq!(pull["base_branch"], "feature-1");
    let reply = alice
        .form_reply(
            "/alice/proj/pulls/2/merge",
            &[
                ("expected_state", pulls::text(&pull, "state")),
                ("expected_updated", pulls::text(&pull, "updated_at")),
                ("expected_head", pulls::text(&pull, "head_sha")),
                ("expected_base", pulls::text(&pull, "base_sha")),
                ("expected_base_branch", "feature-1"),
                ("expected_target", "main"),
                ("confirm", "merge"),
                ("merge_method", "merge"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text());
    assert_eq!(main_tip(&path), main0);
    assert_eq!(alice.pull(2).await["state"], "open");

    let (status, refused) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/2/merge",
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"], "expected_base_branch": "feature-1", "expected_target": "main"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "revision_conflict", "{refused}");
    assert_eq!(main_tip(&path), main0);
    assert_eq!(alice.pull(2).await["state"], "open");

    let (status, merged) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/2/merge",
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"], "expected_base_branch": "feature-1", "expected_target": "feature-1"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert_eq!(alice.pull(2).await["state"], "merged");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_stack_stale_target_is_refused() {
    stack_stale_target_contract(false).await;
}

#[tokio::test]
async fn postgres_stack_stale_target_is_refused() {
    stack_stale_target_contract(true).await;
}
