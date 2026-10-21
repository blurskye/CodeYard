//! Rebase stack on SQLite and PostgreSQL, through the JSON API, the native form and the pull
//! request page. A rebase replays each open layer's own commits onto the layer below it and moves
//! the heads together. It refuses, and moves nothing, when a layer conflicts, a rule forbids a head
//! update, a head is in a fork, or a publication finds a branch that moved meanwhile.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client as pulls;

use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db,
    repositories::store::{repo_store, RestackMove},
    storage::ObjectStore,
};
use database::Database;
use pulls::Client;
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

/// Alice owns `proj`, and bob can read it but not write to it.
struct App {
    alice: Client,
    bob: Client,
    owner: i64,
    repository: db::models::Repository,
}

async fn setup(f: &mut Database, root: &Path) -> App {
    bundle::install();
    f.complete_setup().await;
    let owner = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root,
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob_id,
        repository.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let state = Arc::new(AppState::new(
        f.pool().clone(),
        root.to_owned(),
        Arc::new(ObjectStore::open(root, None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let router = create_router((*state).clone());
    let alice = Client::login(&router, f.pool(), owner).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    App {
        alice,
        bob,
        owner,
        repository,
    }
}

fn git_at(path: &str) -> git2::Repository {
    git2::Repository::open_bare(path).unwrap()
}

fn oid(sha: &str) -> git2::Oid {
    git2::Oid::from_str(sha).unwrap()
}

/// The commit that `branch` points at in the bare repository at `path`.
fn branch_tip(path: &str, branch: &str) -> String {
    git_at(path)
        .refname_to_id(&format!("refs/heads/{branch}"))
        .unwrap()
        .to_string()
}

/// Every reference of the bare repository at `path`, with its commit, sorted by name.
fn refs_of(path: &str) -> Vec<(String, String)> {
    let git = git_at(path);
    let mut refs: Vec<(String, String)> = git
        .references()
        .unwrap()
        .filter_map(|reference| {
            let reference = reference.ok()?;
            Some((
                reference.name()?.to_owned(),
                reference.target()?.to_string(),
            ))
        })
        .collect();
    refs.sort();
    refs
}

/// The first parent of `commit`.
fn parent_of(path: &str, commit: &str) -> String {
    let git = git_at(path);
    let commit = git.find_commit(oid(commit)).unwrap();
    commit.parent_id(0).unwrap().to_string()
}

/// How many commits `head` has that `base` does not.
fn commits_after(path: &str, base: &str, head: &str) -> usize {
    let git = git_at(path);
    let mut walk = git.revwalk().unwrap();
    walk.push(oid(head)).unwrap();
    walk.hide(oid(base)).unwrap();
    walk.count()
}

fn has_file(path: &str, commit: &str, file: &str) -> bool {
    let git = git_at(path);
    let commit = git.find_commit(oid(commit)).unwrap();
    let tree = commit.tree().unwrap();
    tree_has(&tree, file)
}

fn tree_has(tree: &git2::Tree, file: &str) -> bool {
    tree.get_name(file).is_some()
}

/// Asks pull request `number` to join the stack its base branch names.
async fn join(alice: &Client, number: i64) {
    alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/stack"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
}

/// Rebases the stack that pull request `number` is in, from `number` up, through the JSON API.
async fn rebase(client: &Client, number: i64) -> (StatusCode, Value) {
    client
        .api(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/stack/rebase"),
            Value::Null,
        )
        .await
}

/// Three layers, each on the one below, with the top two joined to the stack. Each layer adds one
/// file. Returns the tips of the three layer branches.
async fn three_layers(alice: &Client, main0: &str) -> (String, String, String) {
    alice.branch("feature-1", main0).await;
    let one = alice.save("feature-1", "one.txt", main0).await;
    alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &one).await;
    let two = alice.save("feature-2", "two.txt", &one).await;
    alice.open_pull("Two", "feature-2", "feature-1").await;
    alice.branch("feature-3", &two).await;
    let three = alice.save("feature-3", "three.txt", &two).await;
    alice.open_pull("Three", "feature-3", "feature-2").await;
    join(alice, 2).await;
    join(alice, 3).await;
    (one, two, three)
}

/// Trunk moves on after a stack starts, so every layer is behind it.
async fn move_trunk(alice: &Client, main0: &str) -> String {
    alice.save("main", "main.txt", main0).await
}

async fn restack_moves_each_layer_onto_the_one_below_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    let (one, two, _three) = three_layers(&app.alice, &main0).await;
    let main1 = move_trunk(&app.alice, &main0).await;
    let two_author = git_at(&path)
        .find_commit(oid(&two))
        .unwrap()
        .author()
        .name()
        .unwrap()
        .to_owned();
    let two_when = git_at(&path)
        .find_commit(oid(&two))
        .unwrap()
        .author()
        .when()
        .seconds();

    // Rebasing from the bottom layer moves all three.
    let (status, rebased) = rebase(&app.alice, 1).await;
    assert_eq!(status, StatusCode::OK, "{rebased}");
    assert_eq!(rebased["rebased"], 3);

    let (one2, two2, three2) = (
        branch_tip(&path, "feature-1"),
        branch_tip(&path, "feature-2"),
        branch_tip(&path, "feature-3"),
    );
    assert_ne!(one2, one);
    assert_eq!(parent_of(&path, &one2), main1, "layer 1 sits on trunk");
    assert_eq!(parent_of(&path, &two2), one2, "layer 2 sits on layer 1");
    assert_eq!(parent_of(&path, &three2), two2, "layer 3 sits on layer 2");
    // Each layer had one commit, so each replay wrote exactly one.
    assert_eq!(commits_after(&path, &main1, &three2), 3);
    assert!(has_file(&path, &three2, "main.txt"));
    assert!(has_file(&path, &three2, "one.txt"));
    assert!(has_file(&path, &three2, "two.txt"));
    assert!(has_file(&path, &three2, "three.txt"));
    let git = git_at(&path);
    let replayed = git.find_commit(oid(&two2)).unwrap();
    assert_eq!(replayed.author().name().unwrap(), two_author);
    assert_eq!(replayed.author().when().seconds(), two_when);
    assert_eq!(replayed.summary(), Some("Add two.txt"));

    assert_eq!(app.alice.pull(2).await["head_sha"], two2);
    assert_eq!(app.alice.pull(3).await["head_sha"], three2);

    // The rebased stack merges as one chain, which the stale stack could not do.
    let merged = app.alice.merge_current(3).await;
    assert_eq!(merged["sha"].as_str().unwrap(), branch_tip(&path, "main"));
    for number in [1, 2, 3] {
        assert_eq!(app.alice.pull(number).await["state"], "merged");
    }
    f.finish().await;
}

async fn current_stack_is_a_no_op_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    three_layers(&app.alice, &main0).await;
    let before = refs_of(&path);

    let (status, rebased) = rebase(&app.alice, 1).await;
    assert_eq!(status, StatusCode::OK, "{rebased}");
    assert_eq!(rebased["rebased"], 0);
    assert_eq!(refs_of(&path), before, "no ref moved");
    f.finish().await;
}

async fn middle_conflict_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("feature-1", &main0).await;
    let one = app
        .alice
        .save_text("feature-1", "shared.txt", "A\n", &main0)
        .await;
    app.alice.open_pull("One", "feature-1", "main").await;
    app.alice.branch("feature-2", &one).await;
    let two = app
        .alice
        .update_text("feature-2", "shared.txt", "B\n", &one)
        .await;
    app.alice.open_pull("Two", "feature-2", "feature-1").await;
    app.alice.branch("feature-3", &two).await;
    app.alice
        .save_text("feature-3", "top.txt", "3\n", &two)
        .await;
    app.alice.open_pull("Three", "feature-3", "feature-2").await;
    join(&app.alice, 2).await;
    join(&app.alice, 3).await;
    // The bottom layer changes the line that the middle layer changes, so the middle conflicts.
    app.alice
        .update_text("feature-1", "shared.txt", "X\n", &one)
        .await;
    let before = refs_of(&path);

    let (status, refused) = rebase(&app.alice, 1).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "rebase_conflict", "{refused}");
    let message = refused["message"].as_str().unwrap();
    assert!(message.contains("#2"), "{message}");
    assert!(message.contains("shared.txt"), "{message}");
    assert_eq!(refs_of(&path), before, "no ref moved");
    f.finish().await;
}

async fn protected_head_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    three_layers(&app.alice, &main0).await;
    move_trunk(&app.alice, &main0).await;
    // The rule forbids force updates of feature-2 (its default), and it does not require a pull
    // request, which would refuse any direct write before the force update is judged.
    app.alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({"pattern": "feature-2", "require_pull_request": false}),
            StatusCode::OK,
        )
        .await;
    let before = refs_of(&path);

    let (status, refused) = rebase(&app.alice, 1).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"], "branch_protection", "{refused}");
    assert_eq!(
        refused["message"],
        "Force pushes to this protected branch are blocked."
    );
    assert_eq!(refs_of(&path), before, "no ref moved");
    f.finish().await;
}

/// The head, base and state that the API shows for pull requests 1 to 3.
async fn pull_rows(client: &Client) -> Vec<(Value, Value, Value)> {
    let mut rows = Vec::new();
    for number in [1, 2, 3] {
        let pull = client.pull(number).await;
        rows.push((
            pull["head_sha"].clone(),
            pull["base_sha"].clone(),
            pull["state"].clone(),
        ));
    }
    rows
}

/// A head branch whose rule allows force updates but requires a linear history refuses a rebase that
/// would put a merge commit from trunk into its history. The refusal is the rule's own, and nothing moves.
async fn linear_history_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    three_layers(&app.alice, &main0).await;
    let main1 = move_trunk(&app.alice, &main0).await;
    // A side branch cut from main1 is merged after trunk moves on, so trunk gets a merge commit.
    app.alice.branch("side", &main1).await;
    app.alice.save("side", "side.txt", &main1).await;
    app.alice.open_pull("Side", "side", "main").await;
    app.alice.save("main", "again.txt", &main1).await;
    let (status, merged) = merge_with(&app.alice, 4, "merge").await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    let trunk = branch_tip(&path, "main");
    let git = git_at(&path);
    assert_eq!(git.find_commit(oid(&trunk)).unwrap().parent_count(), 2);

    app.alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({
                "pattern": "feature-1",
                "allow_force_pushes": true,
                "require_linear_history": true,
                "require_pull_request": false,
            }),
            StatusCode::OK,
        )
        .await;
    let before = refs_of(&path);
    let rows_before = pull_rows(&app.alice).await;

    let (status, refused) = rebase(&app.alice, 1).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"], "branch_protection", "{refused}");
    assert_eq!(
        refused["message"],
        "Merge commits are not allowed on this protected branch, which requires a linear history."
    );
    assert_eq!(refs_of(&path), before, "no ref moved");
    assert_eq!(
        pull_rows(&app.alice).await,
        rows_before,
        "no pull request row changed"
    );
    f.finish().await;
}

async fn fork_head_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    three_layers(&app.alice, &main0).await;
    // Trunk moves, so the bottom layer is stale and the rebase reaches the layer in the fork.
    move_trunk(&app.alice, &main0).await;
    // A layer whose head is in a fork is never stacked by the API, so the head repository is set
    // here to a repository that is not this one.
    let fork = app
        .bob
        .expect(
            "POST",
            "/api/repos/alice/proj/forks",
            Value::Null,
            StatusCode::CREATED,
        )
        .await;
    let fork_id = fork["repo"]["id"].as_i64().unwrap();
    f.execute(
        &format!("UPDATE pull_requests SET head_repo_id = {fork_id} WHERE number = 2"),
        &[],
    )
    .await;
    let before = refs_of(&path);

    let (status, refused) = rebase(&app.alice, 1).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "stack_fork_head", "{refused}");
    assert_eq!(refs_of(&path), before, "no ref moved");
    f.finish().await;
}

/// A branch that moves between the preparation and the publication makes the publication stale:
/// nothing moves, and the pushed commit stays where the push put it.
async fn stale_publication_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    let (one, two, _three) = three_layers(&app.alice, &main0).await;
    let main1 = move_trunk(&app.alice, &main0).await;
    let store = repo_store(&app.repository, None);
    let replayed = store
        .prepare_restack(&one, &two, &main1, "Tester", "tester@example.invalid")
        .await
        .unwrap();
    // A push lands on the layer between the preparation and the publication.
    let pushed = app.alice.save("feature-2", "again.txt", &two).await;
    let before = refs_of(&path);
    let stale = store
        .publish_restack_as(
            f.pool(),
            app.repository.id,
            app.owner,
            "feature-1",
            &one,
            &[RestackMove {
                branch: "feature-2".into(),
                old: two.clone(),
                new: replayed.clone(),
            }],
        )
        .await;
    assert!(stale.is_err(), "a stale head must refuse the publication");
    assert_eq!(branch_tip(&path, "feature-2"), pushed);
    assert_eq!(refs_of(&path), before, "no ref moved");

    // A base that moved is stale as well.
    let stale_base = store
        .publish_restack_as(
            f.pool(),
            app.repository.id,
            app.owner,
            "feature-1",
            &main0,
            &[RestackMove {
                branch: "feature-2".into(),
                old: pushed.clone(),
                new: replayed,
            }],
        )
        .await;
    assert!(
        stale_base.is_err(),
        "a stale base must refuse the publication"
    );
    assert_eq!(refs_of(&path), before, "no ref moved");
    f.finish().await;
}

async fn write_access_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    three_layers(&app.alice, &main0).await;
    move_trunk(&app.alice, &main0).await;
    let before = refs_of(&path);

    let (status, refused) = rebase(&app.bob, 1).await;
    assert!(status.is_client_error(), "{status}: {refused}");
    assert_eq!(refs_of(&path), before, "no ref moved");
    f.finish().await;
}

async fn native_form_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    three_layers(&app.alice, &main0).await;
    let main1 = move_trunk(&app.alice, &main0).await;

    let reply = app
        .alice
        .form_reply("/alice/proj/pulls/1/stack/rebase", &[])
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert!(
        reply
            .location
            .as_deref()
            .is_some_and(|to| to.ends_with("/alice/proj/pulls/1")),
        "{:?}",
        reply.location
    );
    assert_eq!(parent_of(&path, &branch_tip(&path, "feature-1")), main1);
    assert_eq!(
        parent_of(&path, &branch_tip(&path, "feature-2")),
        branch_tip(&path, "feature-1")
    );
    f.finish().await;
}

/// The page offers Rebase stack to a writer when a layer is behind, and to nobody else.
async fn rebase_button_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    three_layers(&app.alice, &main0).await;
    let action = "action=\"/alice/proj/pulls/2/stack/rebase\"";

    let (status, page) = app.alice.page("/alice/proj/pulls/2").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains(action), "a current stack offers no rebase");

    move_trunk(&app.alice, &main0).await;
    let (status, page) = app.alice.page("/alice/proj/pulls/2").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains(action),
        "a stack behind trunk offers a rebase"
    );
    assert!(page.contains("Rebase stack"));
    let (status, page) = app.bob.page("/alice/proj/pulls/2").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains(action), "a reader is not offered a rebase");
    f.finish().await;
}

/// Rebase stack on a middle layer moves the whole stale line from its bottom, and the button goes
/// away. With `native`, the press is the form that the page posts instead of the JSON call.
async fn middle_layer_rebase_contract(pg: bool, native: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    three_layers(&app.alice, &main0).await;
    let main1 = move_trunk(&app.alice, &main0).await;
    let action = "action=\"/alice/proj/pulls/2/stack/rebase\"";
    let (status, page) = app.alice.page("/alice/proj/pulls/2").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains(action),
        "a stale line offers the rebase on its middle layer"
    );

    if native {
        let reply = app
            .alice
            .form_reply("/alice/proj/pulls/2/stack/rebase", &[])
            .await;
        assert!(reply.status.is_redirection(), "{}", reply.text());
        assert!(
            reply
                .location
                .as_deref()
                .is_some_and(|to| to.ends_with("/alice/proj/pulls/2")),
            "{:?}",
            reply.location
        );
    } else {
        let (status, rebased) = rebase(&app.alice, 2).await;
        assert_eq!(status, StatusCode::OK, "{rebased}");
        assert_eq!(rebased["rebased"], 3);
    }
    let (one2, two2, three2) = (
        branch_tip(&path, "feature-1"),
        branch_tip(&path, "feature-2"),
        branch_tip(&path, "feature-3"),
    );
    assert_eq!(
        parent_of(&path, &one2),
        main1,
        "the bottom layer moved onto trunk"
    );
    assert_eq!(parent_of(&path, &two2), one2);
    assert_eq!(parent_of(&path, &three2), two2);

    let (status, page) = app.alice.page("/alice/proj/pulls/2").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains(action), "a current line offers no rebase");
    let merged = app.alice.merge_current(3).await;
    assert_eq!(merged["sha"].as_str().unwrap(), branch_tip(&path, "main"));
    for number in [1, 2, 3] {
        assert_eq!(app.alice.pull(number).await["state"], "merged");
    }
    f.finish().await;
}

/// A failed read of the staleness hint hides the button and keeps the page. Feature-3's ref names a
/// commit that does not exist, so the ancestry check of the top layer fails while the line is current.
async fn restack_hint_failure_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    three_layers(&app.alice, &main0).await;
    std::fs::write(
        std::path::Path::new(&path).join("refs/heads/feature-3"),
        format!("{}\n", "1".repeat(40)),
    )
    .unwrap();
    let (status, page) = app.alice.page("/alice/proj/pulls/2").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        !page.contains("Rebase stack"),
        "a failed hint shows no button"
    );
    f.finish().await;
}

/// Merges pull request `number` through the JSON API with `method` at the revisions it shows now.
async fn merge_with(client: &Client, number: i64, method: &str) -> (StatusCode, Value) {
    let pull = client.pull(number).await;
    client
        .api(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/merge"),
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"], "merge_method": method}),
        )
        .await
}

/// Merges the tip of `main` into `branch` in the bare repository at `path`, as an update from trunk
/// that is pushed to the branch.
fn merge_trunk_into(path: &str, branch: &str) {
    let git = git_at(path);
    let ours = git.find_commit(oid(&branch_tip(path, branch))).unwrap();
    let theirs = git.find_commit(oid(&branch_tip(path, "main"))).unwrap();
    let mut index = git.merge_commits(&ours, &theirs, None).unwrap();
    let tree = git.find_tree(index.write_tree_to(&git).unwrap()).unwrap();
    let signature = git2::Signature::now("alice", "alice@example.invalid").unwrap();
    git.commit(
        Some(&format!("refs/heads/{branch}")),
        &signature,
        &signature,
        "Merge main into the branch",
        &tree,
        &[&ours, &theirs],
    )
    .unwrap();
}

/// Points `branch` at a new commit on `parent` that writes `file`, as an amend of its tip does. The
/// new commit does not contain the old tip, so the layer above it is not built on it any more.
fn amend(path: &str, branch: &str, parent: &str, file: &str, content: &str) -> String {
    let git = git_at(path);
    let parent = git.find_commit(oid(parent)).unwrap();
    let blob = git.blob(content.as_bytes()).unwrap();
    let mut builder = git.treebuilder(Some(&parent.tree().unwrap())).unwrap();
    builder.insert(file, blob, 0o100644).unwrap();
    let tree = git.find_tree(builder.write().unwrap()).unwrap();
    let signature = git2::Signature::now("alice", "alice@example.invalid").unwrap();
    let commit = git
        .commit(None, &signature, &signature, "Amend one", &tree, &[&parent])
        .unwrap();
    git.reference(&format!("refs/heads/{branch}"), commit, true, "amend")
        .unwrap();
    commit.to_string()
}

/// The layer below is amended, so its base changes to a commit that does not contain the old one.
/// The layer above still sits on the old commit: its restack must replay its own commit only, and
/// the layer below's old commit must not conflict with the amended file.
async fn amended_layer_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("feature-1", &main0).await;
    let one = app.alice.save("feature-1", "one.txt", &main0).await;
    app.alice.open_pull("One", "feature-1", "main").await;
    app.alice.branch("feature-2", &one).await;
    let two = app.alice.save("feature-2", "two.txt", &one).await;
    app.alice.open_pull("Two", "feature-2", "feature-1").await;
    join(&app.alice, 2).await;

    let amended = amend(&path, "feature-1", &main0, "one.txt", "amended\n");
    // A write on the amended branch refreshes the layer above, which moves its base to that write.
    let base = app.alice.save("feature-1", "extra.txt", &amended).await;
    assert_eq!(app.alice.pull(2).await["base_sha"], base);

    let (status, rebased) = rebase(&app.alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{rebased}");
    assert_eq!(rebased["rebased"], 1);
    let moved = branch_tip(&path, "feature-2");
    assert_ne!(moved, two);
    assert_eq!(parent_of(&path, &moved), base);
    assert_eq!(commits_after(&path, &base, &moved), 1);
    assert!(has_file(&path, &moved, "two.txt"));
    assert!(has_file(&path, &moved, "extra.txt"));
    f.finish().await;
}

/// Squash-merging the bottom layer leaves the layer above it behind trunk, and that layer's merge is
/// refused until it is rebased. Once trunk is merged into it, the layer contains the current tip of
/// its base: the hint shows no button, the rebase moves nothing, and the merge goes through.
async fn updated_stack_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("feature-1", &main0).await;
    let one = app.alice.save("feature-1", "one.txt", &main0).await;
    app.alice.open_pull("One", "feature-1", "main").await;
    app.alice.branch("feature-2", &one).await;
    app.alice.save("feature-2", "two.txt", &one).await;
    app.alice.open_pull("Two", "feature-2", "feature-1").await;
    join(&app.alice, 2).await;
    let action = "action=\"/alice/proj/pulls/2/stack/rebase\"";

    let (status, squashed) = merge_with(&app.alice, 1, "squash").await;
    assert_eq!(status, StatusCode::OK, "{squashed}");
    let (status, refused) = merge_with(&app.alice, 2, "merge").await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "stack_needs_restack", "{refused}");
    let (status, page) = app.alice.page("/alice/proj/pulls/2").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(action), "the stale layer offers the rebase");

    merge_trunk_into(&path, "feature-2");
    let (status, page) = app.alice.page("/alice/proj/pulls/2").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !page.contains(action),
        "a layer that contains the trunk tip offers no rebase"
    );
    let (status, rebased) = rebase(&app.alice, 2).await;
    assert_eq!(status, StatusCode::OK, "{rebased}");
    assert_eq!(rebased["rebased"], 0);

    // The branch was pushed without the record being refreshed, so the merge names its new head.
    let (status, merged) = app
        .alice
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/2/merge",
            json!({
                "expected_head": branch_tip(&path, "feature-2"),
                "expected_base": branch_tip(&path, "main"),
                "merge_method": "merge",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    let main = branch_tip(&path, "main");
    assert!(
        has_file(&path, &main, "two.txt"),
        "the layer's content reaches trunk"
    );
    assert!(has_file(&path, &main, "one.txt"));
    assert_eq!(app.alice.pull(2).await["state"], "merged");
    f.finish().await;
}

/// A merged layer offers no Rebase stack button, and a rebase of it is refused as not open. Its own
/// head is behind trunk after the squash, which the button used to answer for.
async fn merged_layer_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    three_layers_two(&app.alice, &main0).await;
    join(&app.alice, 2).await;
    let (status, squashed) = merge_with(&app.alice, 1, "squash").await;
    assert_eq!(status, StatusCode::OK, "{squashed}");

    let (status, page) = app.alice.page("/alice/proj/pulls/1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !page.contains("Rebase stack"),
        "a merged layer offers no rebase"
    );
    let (status, page) = app.alice.page("/alice/proj/pulls/2").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("Rebase stack"),
        "the open layer above it still offers the rebase"
    );
    let (status, refused) = rebase(&app.alice, 1).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"], "invalid_state", "{refused}");
    f.finish().await;
}

/// Two layers, the top one based on the bottom one's branch, each adding one file.
async fn three_layers_two(alice: &Client, main0: &str) {
    alice.branch("feature-1", main0).await;
    let one = alice.save("feature-1", "one.txt", main0).await;
    alice.open_pull("One", "feature-1", "main").await;
    alice.branch("feature-2", &one).await;
    alice.save("feature-2", "two.txt", &one).await;
    alice.open_pull("Two", "feature-2", "feature-1").await;
}

#[tokio::test]
async fn sqlite_restack_moves_each_layer_onto_the_one_below() {
    restack_moves_each_layer_onto_the_one_below_contract(false).await;
}

#[tokio::test]
async fn postgres_restack_moves_each_layer_onto_the_one_below() {
    restack_moves_each_layer_onto_the_one_below_contract(true).await;
}

#[tokio::test]
async fn sqlite_restack_of_a_current_stack_changes_nothing() {
    current_stack_is_a_no_op_contract(false).await;
}

#[tokio::test]
async fn postgres_restack_of_a_current_stack_changes_nothing() {
    current_stack_is_a_no_op_contract(true).await;
}

#[tokio::test]
async fn sqlite_restack_conflict_names_the_layer_and_moves_nothing() {
    middle_conflict_contract(false).await;
}

#[tokio::test]
async fn postgres_restack_conflict_names_the_layer_and_moves_nothing() {
    middle_conflict_contract(true).await;
}

#[tokio::test]
async fn sqlite_restack_refuses_a_protected_head_with_its_rule() {
    protected_head_contract(false).await;
}

#[tokio::test]
async fn postgres_restack_refuses_a_protected_head_with_its_rule() {
    protected_head_contract(true).await;
}

#[tokio::test]
async fn sqlite_restack_refuses_a_linear_history_rule_with_its_message() {
    linear_history_contract(false).await;
}

#[tokio::test]
async fn postgres_restack_refuses_a_linear_history_rule_with_its_message() {
    linear_history_contract(true).await;
}

#[tokio::test]
async fn sqlite_restack_refuses_a_layer_whose_head_is_in_a_fork() {
    fork_head_contract(false).await;
}

#[tokio::test]
async fn postgres_restack_refuses_a_layer_whose_head_is_in_a_fork() {
    fork_head_contract(true).await;
}

#[tokio::test]
async fn sqlite_restack_publication_is_stale_when_a_branch_moved() {
    stale_publication_contract(false).await;
}

#[tokio::test]
async fn postgres_restack_publication_is_stale_when_a_branch_moved() {
    stale_publication_contract(true).await;
}

#[tokio::test]
async fn sqlite_restack_needs_write_access() {
    write_access_contract(false).await;
}

#[tokio::test]
async fn postgres_restack_needs_write_access() {
    write_access_contract(true).await;
}

#[tokio::test]
async fn sqlite_restack_native_form_redirects_to_the_pull_request() {
    native_form_contract(false).await;
}

#[tokio::test]
async fn postgres_restack_native_form_redirects_to_the_pull_request() {
    native_form_contract(true).await;
}

#[tokio::test]
async fn sqlite_rebase_stack_button_follows_the_stack_state() {
    rebase_button_contract(false).await;
}

#[tokio::test]
async fn postgres_rebase_stack_button_follows_the_stack_state() {
    rebase_button_contract(true).await;
}

#[tokio::test]
async fn sqlite_rebase_from_a_middle_layer_moves_the_whole_line() {
    middle_layer_rebase_contract(false, false).await;
}

#[tokio::test]
async fn postgres_rebase_from_a_middle_layer_moves_the_whole_line() {
    middle_layer_rebase_contract(true, false).await;
}

#[tokio::test]
async fn sqlite_native_rebase_from_a_middle_layer_moves_the_whole_line() {
    middle_layer_rebase_contract(false, true).await;
}

#[tokio::test]
async fn postgres_native_rebase_from_a_middle_layer_moves_the_whole_line() {
    middle_layer_rebase_contract(true, true).await;
}

#[tokio::test]
async fn sqlite_restack_hint_failure_keeps_the_page() {
    restack_hint_failure_contract(false).await;
}

#[tokio::test]
async fn postgres_restack_hint_failure_keeps_the_page() {
    restack_hint_failure_contract(true).await;
}

#[tokio::test]
async fn sqlite_updated_stack_merges_after_trunk_is_merged_into_it() {
    updated_stack_contract(false).await;
}

#[tokio::test]
async fn postgres_updated_stack_merges_after_trunk_is_merged_into_it() {
    updated_stack_contract(true).await;
}

#[tokio::test]
async fn sqlite_merged_layer_offers_no_rebase() {
    merged_layer_contract(false).await;
}

#[tokio::test]
async fn postgres_merged_layer_offers_no_rebase() {
    merged_layer_contract(true).await;
}

async fn slow_ci_admission_contract(restack: bool) {
    let mut f = Database::new(true).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    f.execute(
        "UPDATE users SET is_admin = TRUE WHERE id = $1",
        &[app.owner],
    )
    .await;
    let main = app.alice.save("main", "README.md", "unborn").await;
    let main = app.alice.save_text("main", ".github/workflows/ci.yml", "on: [pull_request, push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n", &main).await;
    app.alice.branch("feature-1", &main).await;
    let one = app.alice.save("feature-1", "one.txt", &main).await;
    app.alice.open_draft("One", "feature-1", "main").await;
    app.alice.branch("feature-2", &one).await;
    app.alice.save("feature-2", "two.txt", &one).await;
    app.alice.open_draft("Two", "feature-2", "feature-1").await;
    join(&app.alice, 2).await;
    app.alice.save("main", "new.txt", &main).await;
    f.execute("UPDATE pull_requests SET is_draft = FALSE", &[])
        .await;
    let pool = f.pg();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE ci_runs IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let release = async {
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        lock.commit().await.unwrap();
    };
    let admission = async {
        if restack {
            let (status, result) = rebase(&app.alice, 2).await;
            assert_eq!(status, StatusCode::OK, "{result}");
        } else {
            let pull = db::pull_requests::records::get_pull_request(f.pool(), app.repository.id, 2)
                .await
                .unwrap()
                .unwrap();
            let ci = codeyard::backend::ci::CiRuns::new(
                f.pool().clone(),
                Arc::new(CiConfig::default()),
                Arc::new(tokio::sync::Notify::new()),
                None,
                Arc::new(codeyard::backend::notifications::NotificationBroadcaster::new(f.pool())),
            );
            codeyard::backend::repositories::pull_requests::workflows::queue_moved_pull_requests_ci(
                f.pool(), &ci, &app.repository, &[pull], app.owner).await;
        }
    };
    tokio::join!(admission, release);
    let head = branch_tip(&app.repository.disk_path, "feature-2");
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), app.repository.id, 100)
        .await
        .unwrap();
    assert!(
        runs.iter().any(|run| run.event == "pull_request"
            && run.pull_request_id.is_some()
            && run.head_sha.as_deref() == Some(&head)),
        "{runs:?}"
    );
    if restack {
        assert!(
            runs.iter().any(|run| run.event == "push"
                && run.branch == "feature-2"
                && run.head_sha.as_deref() == Some(&head)),
            "{runs:?}"
        );
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_slow_restack_admission_finishes_after_five_seconds() {
    slow_ci_admission_contract(true).await;
}
#[tokio::test]
async fn postgres_slow_moved_pr_admission_finishes_after_five_seconds() {
    slow_ci_admission_contract(false).await;
}

async fn nearer_fork_beats_older_merged_ancestor(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = &app.repository.disk_path;
    let main = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("a", &main).await;
    let a = app.alice.save("a", "a.txt", &main).await;
    app.alice.open_pull("A", "a", "main").await;
    app.alice.branch("b", &a).await;
    let b = app.alice.save("b", "obsolete.txt", &a).await;
    app.alice.open_pull("B", "b", "a").await;
    join(&app.alice, 2).await;
    app.alice.branch("c", &b).await;
    app.alice.save("c", "child.txt", &b).await;
    app.alice.open_pull("C", "c", "b").await;
    join(&app.alice, 3).await;
    let (status, result) = merge_with(&app.alice, 1, "squash").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let rewritten = amend(
        path,
        "b",
        &branch_tip(path, "main"),
        "replacement.txt",
        "new\n",
    );
    app.alice.save("b", "extra.txt", &rewritten).await;
    let (status, result) = merge_with(&app.alice, 2, "squash").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let child = db::pull_requests::records::get_pull_request(f.pool(), app.repository.id, 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.fork_sha.as_deref(), Some(b.as_str()));
    let (status, result) = rebase(&app.alice, 3).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let head = branch_tip(path, "c");
    assert!(has_file(path, &head, "child.txt"));
    assert!(has_file(path, &head, "replacement.txt"));
    assert!(
        !has_file(path, &head, "obsolete.txt"),
        "older merged ancestor replayed superseded B changes"
    );
    f.finish().await;
}

async fn missing_replay_boundary(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let main = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("lower", &main).await;
    let old = app.alice.save("lower", "obsolete.txt", &main).await;
    app.alice.open_pull("Lower", "lower", "main").await;
    app.alice.branch("upper", &old).await;
    app.alice.save("upper", "child.txt", &old).await;
    let rewritten = amend(
        &app.repository.disk_path,
        "lower",
        &main,
        "replacement.txt",
        "new\n",
    );
    app.alice.save("lower", "extra.txt", &rewritten).await;
    // The upper PR never observed the lower head it was actually built on.
    app.alice.open_pull("Upper", "upper", "lower").await;
    join(&app.alice, 2).await;
    let before = refs_of(&app.repository.disk_path);
    let (status, result) = rebase(&app.alice, 2).await;
    assert_eq!(status, StatusCode::CONFLICT, "{result}");
    assert_eq!(result["error"], "unknown_rebase_boundary");
    assert_eq!(refs_of(&app.repository.disk_path), before);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_recorded_fork_wins_over_older_merged_ancestor() {
    nearer_fork_beats_older_merged_ancestor(false).await;
}
#[tokio::test]
async fn postgres_recorded_fork_wins_over_older_merged_ancestor() {
    nearer_fork_beats_older_merged_ancestor(true).await;
}
#[tokio::test]
async fn sqlite_restack_refuses_an_unknown_own_commit_boundary() {
    missing_replay_boundary(false).await;
}
#[tokio::test]
async fn postgres_restack_refuses_an_unknown_own_commit_boundary() {
    missing_replay_boundary(true).await;
}

async fn rewrite_boundary_contract(pg: bool, mode: &str) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = &app.repository.disk_path;
    let main = app.alice.save("main", "README.md", "unborn").await;
    let lower = if mode == "bottom" {
        "main"
    } else {
        "feature-1"
    };
    if lower != "main" {
        app.alice.branch(lower, &main).await;
    }
    let old = app.alice.save(lower, "obsolete.txt", &main).await;
    if lower != "main" {
        app.alice.open_pull("Lower", lower, "main").await;
    }
    app.alice.branch("upper", &old).await;
    let upper = app.alice.save("upper", "child.txt", &old).await;
    app.alice.open_pull("Upper", "upper", lower).await;
    if mode == "bottom" {
        app.alice.branch("top", &upper).await;
        app.alice.save("top", "top.txt", &upper).await;
        app.alice.open_pull("Top", "top", "upper").await;
        join(&app.alice, 2).await;
    } else if mode != "late_join" {
        join(&app.alice, 2).await;
    }
    let rewritten = amend(path, lower, &main, "replacement.txt", "new\n");
    app.alice.save(lower, "extra.txt", &rewritten).await;
    if mode == "late_join" {
        join(&app.alice, 2).await;
    }
    if mode == "merged" {
        let (status, result) = merge_with(&app.alice, 1, "squash").await;
        assert_eq!(status, StatusCode::OK, "{result}");
    }
    let number = if mode == "bottom" { 1 } else { 2 };
    let (status, result) = rebase(&app.alice, number).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let head = branch_tip(path, "upper");
    assert!(has_file(path, &head, "child.txt"));
    assert!(has_file(path, &head, "replacement.txt"));
    assert!(
        !has_file(path, &head, "obsolete.txt"),
        "superseded base changes were replayed"
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_join_after_parent_rewrite_keeps_the_replay_boundary() {
    rewrite_boundary_contract(false, "late_join").await;
}
#[tokio::test]
async fn postgres_join_after_parent_rewrite_keeps_the_replay_boundary() {
    rewrite_boundary_contract(true, "late_join").await;
}
#[tokio::test]
async fn sqlite_bottom_restack_keeps_the_recorded_fork() {
    rewrite_boundary_contract(false, "bottom").await;
}
#[tokio::test]
async fn postgres_bottom_restack_keeps_the_recorded_fork() {
    rewrite_boundary_contract(true, "bottom").await;
}
#[tokio::test]
async fn sqlite_merged_amended_parent_keeps_the_child_boundary() {
    rewrite_boundary_contract(false, "merged").await;
}
#[tokio::test]
async fn postgres_merged_amended_parent_keeps_the_child_boundary() {
    rewrite_boundary_contract(true, "merged").await;
}

#[tokio::test]
async fn sqlite_restack_after_the_layer_below_is_amended_replays_only_its_own_commit() {
    amended_layer_contract(false).await;
}

#[tokio::test]
async fn postgres_restack_after_the_layer_below_is_amended_replays_only_its_own_commit() {
    amended_layer_contract(true).await;
}
