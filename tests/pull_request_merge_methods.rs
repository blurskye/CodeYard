//! Merge methods on pull requests, on SQLite and PostgreSQL. A squash or rebase merge writes the
//! commits its method promises, the repository's allowed methods bound what a merger may choose,
//! and a refused merge leaves the base branch and the pull request as they were. The native merge
//! form offers each allowed method, and squashing a bottom layer retargets the layer above it.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

#[path = "support/fake_webui_bundle.rs"]
mod bundle;
#[path = "support/database.rs"]
mod database;
#[path = "editor_save/fixture.rs"]
mod fixture;
#[path = "support/native_database.rs"]
mod native;
#[path = "support/production_ci.rs"]
mod production;
#[path = "support/pull_requests.rs"]
mod pulls;

use axum::http::StatusCode;
use base64::Engine as _;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db,
    repositories::{
        pull_requests::{
            merge::{prepare_pull_request_merge, publish_prepared_merge, MergeRequest},
            OperationError,
        },
        store::MergeMethod,
    },
    storage::ObjectStore,
};
use database::Database;
use pulls::{text, Client, Reply};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

/// Alice owns `proj`, and `merger` can write to it.
struct Fixture {
    state: Arc<AppState>,
    alice: Client,
    merger: Client,
    repository: db::models::Repository,
    merger_id: i64,
    owner: i64,
}

async fn setup(f: &mut Database, root: &Path) -> Fixture {
    bundle::install();
    f.execute(
        "INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE",
        &[],
    )
    .await;
    let owner = f.actor("alice").await;
    let merger_id = f.actor("merger").await;
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
        merger_id,
        repository.id,
        db::models::AccessLevel::Write,
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
    let merger = Client::login(&router, f.pool(), merger_id).await;
    Fixture {
        state,
        alice,
        merger,
        repository,
        merger_id,
        owner,
    }
}

fn oid(sha: &str) -> git2::Oid {
    git2::Oid::from_str(sha).unwrap()
}

/// The commit that `main` points at in the bare repository at `path`.
fn main_tip(path: &str) -> String {
    let git = git2::Repository::open_bare(path).unwrap();
    git.refname_to_id("refs/heads/main").unwrap().to_string()
}

/// Saves `content` as `file` on `branch` through the editor API and returns the new tip.
async fn save_text(client: &Client, branch: &str, file: &str, content: &str, tip: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(content);
    let saved = client
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/contents/{file}"),
            json!({"content": encoded, "message": format!("Add {file}"), "branch": branch, "expected_revision": tip}),
            StatusCode::CREATED,
        )
        .await;
    saved["sha"].as_str().unwrap().to_owned()
}

/// Merges pull request `number` through the JSON API at the revisions it shows now. Without
/// `method` the request leaves the field out, as clients from before merge methods do.
async fn merge(client: &Client, number: i64, method: Option<&str>) -> (StatusCode, Value) {
    let pull = client.pull(number).await;
    let mut body = json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"]});
    if let Some(method) = method {
        body["merge_method"] = json!(method);
    }
    client
        .api(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/merge"),
            body,
        )
        .await
}

/// Overwrites `file` on `branch` through the editor API and returns the new tip.
async fn update_text(
    client: &Client,
    branch: &str,
    file: &str,
    content: &str,
    tip: &str,
) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(content);
    let saved = client
        .expect(
            "PUT",
            &format!("/api/repos/alice/proj/contents/{file}"),
            json!({"content": encoded, "message": format!("Change {file}"), "branch": branch, "expected_revision": tip}),
            StatusCode::OK,
        )
        .await;
    saved["commit"]["sha"].as_str().unwrap().to_owned()
}

/// The bytes of `file` in `commit` of the bare repository at `path`.
fn file_at(path: &str, commit: &str, file: &str) -> Vec<u8> {
    let git = git2::Repository::open_bare(path).unwrap();
    let tree = git.find_commit(oid(commit)).unwrap().tree().unwrap();
    let entry = tree.get_name(file).unwrap();
    let blob = git.find_blob(entry.id()).unwrap();
    blob.content().to_vec()
}

/// Asks pull request `number` to join the stack of the layer its base branch is based on.
async fn join_stack(client: &Client, number: i64) {
    client
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/stack"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
}

/// Closes or reopens pull request `number`, as the pull request page does.
async fn set_state(client: &Client, number: i64, from: &str, to: &str) {
    let pull = client.pull(number).await;
    client
        .expect(
            "PUT",
            &format!("/api/repos/alice/proj/pulls/{number}"),
            json!({"expected_state": from, "expected_updated": pull["updated_at"], "state": to}),
            StatusCode::OK,
        )
        .await;
}

/// Whether the merge form's radio for `method` is checked in `page`.
fn radio_checked(page: &str, method: &str) -> bool {
    let start = page
        .find(&format!("value=\"{method}\""))
        .expect("the merge form offers the method");
    let tag = &page[start..];
    tag[..tag.find('>').unwrap()].contains("checked")
}

/// A native merge that a rule refuses keeps the method that the form submitted, so the form that
/// comes back selects it again.
async fn refused_form_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("feature", &main0).await;
    save_text(&app.alice, "feature", "one.txt", "1\n", &main0).await;
    app.alice.open_pull("One", "feature", "main").await;
    // The rule wants one approval, and nobody has given one.
    db::repositories::protection::upsert_branch_protection_rule(
        f.pool(),
        db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: app.owner,
            repo_id: app.repository.id,
            pattern: "main",
            require_pull_request: false,
            required_approving_reviews: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let reply = merge_form(&app.merger, 1, "squash").await;
    let page = reply.text();
    assert_eq!(reply.status, StatusCode::CONFLICT, "{page}");
    assert!(page.contains("approving review required"), "{page}");
    assert!(
        radio_checked(&page, "squash"),
        "the form that comes back lost squash"
    );
    assert!(
        !radio_checked(&page, "merge"),
        "the form that comes back selects merge"
    );
    f.finish().await;
}

/// Checks that the pull request row records `sha` as its merge commit.
async fn assert_merge_commit(f: &Database, app: &Fixture, number: i64, sha: &str) {
    let row =
        db::pull_requests::records::get_pull_request(f.pool(), app.repository.id, number as i32)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(row.merge_commit_sha.as_deref(), Some(sha));
}

/// Changes the repository's merge settings through the settings API.
async fn allow(client: &Client, settings: Value) {
    client
        .expect("PATCH", "/api/repos/alice/proj", settings, StatusCode::OK)
        .await;
}

/// Posts the merge form that the page shows for pull request `number`, choosing `method`.
async fn merge_form(client: &Client, number: i64, method: &str) -> Reply {
    let pull = client.pull(number).await;
    client
        .form_reply(
            &format!("/alice/proj/pulls/{number}/merge"),
            &[
                ("expected_state", text(&pull, "state")),
                ("expected_updated", text(&pull, "updated_at")),
                ("expected_head", text(&pull, "head_sha")),
                ("expected_base", text(&pull, "base_sha")),
                ("confirm", "merge"),
                ("merge_method", method),
            ],
        )
        .await
}

async fn squash_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    // The author has an account email, which the squash commit must not publish.
    f.execute(
        "UPDATE users SET email = 'alice@private.example' WHERE id = $1",
        &[app.owner],
    )
    .await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    let main1 = app.alice.save("main", "main.txt", &main0).await;
    app.alice.branch("feature", &main1).await;
    let one = app.alice.save("feature", "one.txt", &main1).await;
    let head = app.alice.save("feature", "two.txt", &one).await;
    let pull = app.alice.open_pull("Two files", "feature", "main").await;
    let number = pull["number"].as_i64().unwrap();

    let (status, merged) = merge(&app.merger, number, Some("squash")).await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    let sha = text(&merged, "sha").to_owned();
    let git = git2::Repository::open_bare(&path).unwrap();
    let commit = git.find_commit(oid(&sha)).unwrap();
    assert_eq!(commit.parent_count(), 1);
    assert_eq!(commit.parent_id(0).unwrap().to_string(), main1);
    assert_eq!(
        commit.tree_id(),
        git.find_commit(oid(&head)).unwrap().tree_id()
    );
    // The pull request's author wrote the change, and the merger committed it.
    assert_eq!(commit.author().name(), Some("alice"));
    assert_eq!(commit.author().email(), Some("alice@users.noreply"));
    assert_eq!(commit.committer().name(), Some("merger"));
    let odb = git.odb().unwrap();
    let raw = odb.read(commit.id()).unwrap();
    assert!(
        !String::from_utf8_lossy(raw.data()).contains("alice@private.example"),
        "the squash commit publishes the account email"
    );
    assert_eq!(
        commit.message(),
        Some("Two files (#1)\n\n* Add one.txt\n* Add two.txt\n")
    );
    assert_eq!(main_tip(&path), sha);
    let pull = app.alice.pull(number).await;
    assert_eq!(pull["state"], "merged");
    assert_merge_commit(&f, &app, number, &sha).await;
    f.finish().await;
}

async fn rebase_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    let main1 = app.alice.save("main", "main.txt", &main0).await;
    app.alice.branch("feature", &main1).await;
    let one = app.alice.save("feature", "one.txt", &main1).await;
    let head = app.alice.save("feature", "two.txt", &one).await;
    let pull = app.alice.open_pull("Two files", "feature", "main").await;
    let number = pull["number"].as_i64().unwrap();

    let (status, merged) = merge(&app.merger, number, Some("rebase")).await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    let sha = text(&merged, "sha").to_owned();
    let git = git2::Repository::open_bare(&path).unwrap();
    // The tip replays the second commit, and its parent replays the first.
    let second = git.find_commit(oid(&sha)).unwrap();
    let first = second.parent(0).unwrap();
    assert_eq!(
        second.tree_id(),
        git.find_commit(oid(&head)).unwrap().tree_id()
    );
    assert_eq!(first.parent_id(0).unwrap().to_string(), main1);
    assert_eq!(second.parent_id(0).unwrap(), first.id());
    // Each commit is written again with its own author and date, committed by the merger.
    for (written, original) in [(&first, &one), (&second, &head)] {
        let original = git.find_commit(oid(original)).unwrap();
        assert_ne!(written.id(), original.id());
        assert_eq!(written.summary(), original.summary());
        assert_eq!(written.author().name(), Some("alice"));
        assert_eq!(
            written.author().when().seconds(),
            original.author().when().seconds()
        );
        assert_eq!(written.committer().name(), Some("merger"));
    }
    assert_eq!(main_tip(&path), sha);
    let pull = app.alice.pull(number).await;
    assert_eq!(pull["state"], "merged");
    assert_merge_commit(&f, &app, number, &sha).await;
    f.finish().await;
}

async fn disallowed_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("feature", &main0).await;
    app.alice.save("feature", "one.txt", &main0).await;
    let pull = app.alice.open_pull("One", "feature", "main").await;
    let number = pull["number"].as_i64().unwrap();

    allow(
        &app.alice,
        json!({"is_private": false, "allow_squash_merge": false}),
    )
    .await;
    let (status, refused) = merge(&app.merger, number, Some("squash")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "merge_method_not_allowed", "{refused}");
    assert_eq!(main_tip(&path), main0);
    assert_eq!(app.alice.pull(number).await["state"], "open");
    // The form offers only the methods that are allowed.
    let (status, page) = app
        .merger
        .page(&format!("/alice/proj/pulls/{number}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !page.contains(r#"value="squash""#),
        "the form offers squash"
    );
    assert!(page.contains(r#"value="rebase""#), "the form lacks rebase");
    // A merge commit is still allowed, and a request that leaves the method out still makes one.
    let (status, merged) = merge(&app.merger, number, None).await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    f.finish().await;
}

async fn merge_commit_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("feature", &main0).await;
    let one = app.alice.save("feature", "one.txt", &main0).await;
    // The head branch gets a merge commit, which rebase merging cannot replay.
    let git = git2::Repository::open_bare(&path).unwrap();
    let signature = git2::Signature::now("Tester", "tester@example.invalid").unwrap();
    let base_commit = git.find_commit(oid(&main0)).unwrap();
    let side = git
        .commit(
            Some("refs/heads/side"),
            &signature,
            &signature,
            "Side",
            &base_commit.tree().unwrap(),
            &[&base_commit],
        )
        .unwrap();
    let side_commit = git.find_commit(side).unwrap();
    let one_commit = git.find_commit(oid(&one)).unwrap();
    git.commit(
        Some("refs/heads/feature"),
        &signature,
        &signature,
        "Merge side",
        &one_commit.tree().unwrap(),
        &[&one_commit, &side_commit],
    )
    .unwrap();
    let pull = app.alice.open_pull("Merged head", "feature", "main").await;
    let number = pull["number"].as_i64().unwrap();

    let (status, refused) = merge(&app.merger, number, Some("rebase")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "unsupported_merge", "{refused}");
    assert!(
        text(&refused, "message").contains("merge commits"),
        "{refused}"
    );
    assert_eq!(main_tip(&path), main0);
    assert_eq!(app.alice.pull(number).await["state"], "open");
    f.finish().await;
}

async fn conflict_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("feature", &main0).await;
    save_text(&app.alice, "feature", "shared.txt", "feature\n", &main0).await;
    let main1 = save_text(&app.alice, "main", "shared.txt", "main\n", &main0).await;
    let pull = app.alice.open_pull("Shared", "feature", "main").await;
    let number = pull["number"].as_i64().unwrap();

    for method in ["squash", "rebase"] {
        let (status, refused) = merge(&app.merger, number, Some(method)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method}: {refused}");
        assert_eq!(refused["error"], "merge_conflict", "{method}: {refused}");
        assert!(
            text(&refused, "message").contains("shared.txt"),
            "{method}: {refused}"
        );
        assert_eq!(main_tip(&path), main1);
        assert_eq!(app.alice.pull(number).await["state"], "open");
    }
    f.finish().await;
}

async fn native_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let mut main = app.alice.save("main", "README.md", "unborn").await;
    for method in ["merge", "squash", "rebase"] {
        let branch = format!("form-{method}");
        app.alice.branch(&branch, &main).await;
        let one = app
            .alice
            .save(&branch, &format!("{method}-1.txt"), &main)
            .await;
        app.alice
            .save(&branch, &format!("{method}-2.txt"), &one)
            .await;
        let pull = app
            .alice
            .open_pull(&format!("Form {method}"), &branch, "main")
            .await;
        let number = pull["number"].as_i64().unwrap();
        // The base moves on, so a merge commit is the only way the merge method can differ.
        app.alice
            .save("main", &format!("{method}-base.txt"), &main)
            .await;

        let (status, page) = app
            .merger
            .page(&format!("/alice/proj/pulls/{number}"))
            .await;
        assert_eq!(status, StatusCode::OK);
        for allowed in ["merge", "squash", "rebase"] {
            assert!(
                page.contains(&format!(r#"value="{allowed}""#)),
                "{method}: the form lacks {allowed}"
            );
        }
        // A plain form post, without JavaScript, merges with the chosen method.
        let reply = merge_form(&app.merger, number, method).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
        let pull = app.alice.pull(number).await;
        assert_eq!(pull["state"], "merged", "{method}");
        let git = git2::Repository::open_bare(&path).unwrap();
        let tip = git.find_commit(oid(&main_tip(&path))).unwrap();
        let expected_parents = if method == "merge" { 2 } else { 1 };
        assert_eq!(tip.parent_count(), expected_parents, "{method}");
        main = main_tip(&path);
    }
    f.finish().await;
}

async fn default_method_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.save("main", "main.txt", &main0).await;
    app.alice.branch("feature", &main0).await;
    app.alice.save("feature", "one.txt", &main0).await;
    let pull = app.alice.open_pull("One", "feature", "main").await;
    let number = pull["number"].as_i64().unwrap();

    let (status, merged) = merge(&app.merger, number, None).await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    let git = git2::Repository::open_bare(&path).unwrap();
    let commit = git.find_commit(oid(text(&merged, "sha"))).unwrap();
    assert_eq!(commit.parent_count(), 2);
    assert_eq!(
        commit.summary(),
        Some(format!("Merge pull request #{number} from feature").as_str())
    );
    f.finish().await;
}

async fn prepared_merge_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    let main1 = app.alice.save("main", "main.txt", &main0).await;
    app.alice.branch("feature", &main0).await;
    let head = app.alice.save("feature", "one.txt", &main0).await;
    let pull = app.alice.open_pull("One", "feature", "main").await;
    let number = pull["number"].as_i64().unwrap() as i32;
    let request = |head: &str, base: &str| MergeRequest {
        expected_head: head.to_owned(),
        expected_base: base.to_owned(),
        expected_base_branch: None,
        expected_target: None,
        merge_method: MergeMethod::Squash,
    };

    // A setting that changes after the merge was prepared is the one that decides.
    let prepared = prepare_pull_request_merge(
        f.pool(),
        &app.state.ci_runs,
        app.merger_id,
        ("alice".to_owned(), "proj".to_owned(), number),
        request(head.as_str(), main1.as_str()),
    )
    .await
    .unwrap();
    allow(
        &app.alice,
        json!({"is_private": false, "allow_squash_merge": false}),
    )
    .await;
    let refused = publish_prepared_merge(
        f.pool(),
        &app.state.ci_runs,
        &app.state.notification_broadcaster,
        prepared,
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(OperationError::Conflict {
                code: "merge_method_not_allowed",
                ..
            })
        ),
        "a method turned off after preparation was published"
    );
    assert_eq!(main_tip(&path), main1);
    assert_eq!(app.alice.pull(number as i64).await["state"], "open");

    // A head that moves after preparation refuses the publish.
    allow(
        &app.alice,
        json!({"is_private": false, "allow_squash_merge": true}),
    )
    .await;
    let prepared = prepare_pull_request_merge(
        f.pool(),
        &app.state.ci_runs,
        app.merger_id,
        ("alice".to_owned(), "proj".to_owned(), number),
        request(head.as_str(), main1.as_str()),
    )
    .await
    .unwrap();
    app.alice.save("feature", "two.txt", &head).await;
    let moved = publish_prepared_merge(
        f.pool(),
        &app.state.ci_runs,
        &app.state.notification_broadcaster,
        prepared,
    )
    .await;
    assert!(
        matches!(
            moved,
            Err(OperationError::Conflict {
                code: "revision_conflict",
                ..
            })
        ),
        "a moved head was published"
    );
    assert_eq!(main_tip(&path), main1);
    assert_eq!(app.alice.pull(number as i64).await["state"], "open");
    f.finish().await;
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

/// A stack whose bottom layer adds `shared.txt` and whose top layer changes it. Squashing the bottom
/// lands its content on trunk and moves the top onto trunk. The top's own commit still sits on the
/// bottom's old head, so the top is stale: its merge asks for a rebase, and the rebase replays that
/// commit onto the squashed trunk. Then the top merges.
async fn stacked_bottom_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("feature-1", &main0).await;
    let one = save_text(&app.alice, "feature-1", "shared.txt", "A\n", &main0).await;
    app.alice.open_pull("One", "feature-1", "main").await;
    app.alice.branch("feature-2", &one).await;
    update_text(&app.alice, "feature-2", "shared.txt", "B\n", &one).await;
    app.alice.open_pull("Two", "feature-2", "feature-1").await;
    app.alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/2/stack",
            Value::Null,
            StatusCode::OK,
        )
        .await;

    // Nothing is stacked below the bottom layer, so the form offers every method.
    let (status, page) = app.merger.page("/alice/proj/pulls/1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !page.contains("Squash and rebase are unavailable"),
        "the bottom layer with a layer above it still offers squash and rebase"
    );
    let (status, squashed) = merge(&app.merger, 1, Some("squash")).await;
    assert_eq!(status, StatusCode::OK, "{squashed}");
    assert_eq!(
        file_at(&path, &main_tip(&path), "shared.txt"),
        b"A\n".to_vec()
    );
    assert_eq!(app.alice.pull(2).await["base_branch"], "main");

    let (status, refused) = merge(&app.merger, 2, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "stack_needs_restack", "{refused}");
    let (status, rebased) = rebase(&app.merger, 2).await;
    assert_eq!(status, StatusCode::OK, "{rebased}");
    assert_eq!(rebased["rebased"], 1);
    let (status, merged) = merge(&app.merger, 2, None).await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert_eq!(
        file_at(&path, &main_tip(&path), "shared.txt"),
        b"B\n".to_vec()
    );
    f.finish().await;
}

/// Two children of one parent, opened before it so that the parent has the highest number. The
/// first child joins, closes, the second child joins, and the first reopens. Squashing the parent
/// lands it on trunk and moves both children onto trunk. Each child then rebases its own commits
/// and merges.
async fn branching_stack_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("feature-1", &main0).await;
    let parent = save_text(&app.alice, "feature-1", "parent.txt", "P\n", &main0).await;
    app.alice.branch("feature-2a", &parent).await;
    save_text(&app.alice, "feature-2a", "a.txt", "A\n", &parent).await;
    app.alice.open_pull("A", "feature-2a", "feature-1").await;
    app.alice.branch("feature-2b", &parent).await;
    save_text(&app.alice, "feature-2b", "b.txt", "B\n", &parent).await;
    app.alice.open_pull("B", "feature-2b", "feature-1").await;
    app.alice.open_pull("Parent", "feature-1", "main").await;
    join_stack(&app.alice, 1).await;
    set_state(&app.alice, 1, "open", "closed").await;
    join_stack(&app.alice, 2).await;
    set_state(&app.alice, 1, "closed", "open").await;

    let (status, squashed) = merge(&app.merger, 3, Some("squash")).await;
    assert_eq!(status, StatusCode::OK, "{squashed}");
    for number in [1, 2] {
        assert_eq!(app.alice.pull(number).await["base_branch"], "main");
        let (status, refused) = merge(&app.merger, number, Some("merge")).await;
        assert_eq!(status, StatusCode::CONFLICT, "#{number}: {refused}");
        assert_eq!(
            refused["error"], "stack_needs_restack",
            "#{number}: {refused}"
        );
        let (status, rebased) = rebase(&app.merger, number).await;
        assert_eq!(status, StatusCode::OK, "#{number}: {rebased}");
        assert_eq!(rebased["rebased"], 1);
    }
    for number in [1, 2] {
        let (status, merged) = merge(&app.merger, number, Some("merge")).await;
        assert_eq!(status, StatusCode::OK, "#{number}: {merged}");
    }
    assert_eq!(file_at(&path, &main_tip(&path), "a.txt"), b"A\n".to_vec());
    assert_eq!(file_at(&path, &main_tip(&path), "b.txt"), b"B\n".to_vec());
    f.finish().await;
}

/// A three-layer stack. Squashing the bottom lands it on trunk and moves the middle onto trunk,
/// which leaves the middle needing a rebase. Rebasing from the middle replays the middle and the
/// top onto trunk. Then the middle merges with a merge commit, and the top squashes.
async fn middle_layer_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("feature-1", &main0).await;
    let bottom = save_text(&app.alice, "feature-1", "bottom.txt", "1\n", &main0).await;
    app.alice.open_pull("Bottom", "feature-1", "main").await;
    app.alice.branch("feature-2", &bottom).await;
    let middle = save_text(&app.alice, "feature-2", "middle.txt", "2\n", &bottom).await;
    app.alice
        .open_pull("Middle", "feature-2", "feature-1")
        .await;
    app.alice.branch("feature-3", &middle).await;
    save_text(&app.alice, "feature-3", "top.txt", "3\n", &middle).await;
    app.alice.open_pull("Top", "feature-3", "feature-2").await;
    join_stack(&app.alice, 2).await;
    join_stack(&app.alice, 3).await;

    let (status, squashed) = merge(&app.merger, 1, Some("squash")).await;
    assert_eq!(status, StatusCode::OK, "{squashed}");
    let (status, refused) = merge(&app.merger, 2, Some("merge")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "stack_needs_restack", "{refused}");
    let (status, rebased) = rebase(&app.merger, 2).await;
    assert_eq!(status, StatusCode::OK, "{rebased}");
    assert_eq!(rebased["rebased"], 2);
    let (status, merged) = merge(&app.merger, 2, Some("merge")).await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    let (status, squashed) = merge(&app.merger, 3, Some("squash")).await;
    assert_eq!(status, StatusCode::OK, "{squashed}");
    assert_eq!(app.alice.pull(3).await["state"], "merged");
    f.finish().await;
}

/// The top layer of a stack has nothing stacked above it, so it can be squashed once the layer
/// below has merged.
async fn stacked_top_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let path = app.repository.disk_path.clone();
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("feature-1", &main0).await;
    let one = save_text(&app.alice, "feature-1", "shared.txt", "A\n", &main0).await;
    app.alice.open_pull("One", "feature-1", "main").await;
    app.alice.branch("feature-2", &one).await;
    update_text(&app.alice, "feature-2", "shared.txt", "B\n", &one).await;
    app.alice.open_pull("Two", "feature-2", "feature-1").await;
    app.alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/2/stack",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let (status, merged) = merge(&app.merger, 1, Some("merge")).await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    let before = main_tip(&path);
    let (status, squashed) = merge(&app.merger, 2, Some("squash")).await;
    assert_eq!(status, StatusCode::OK, "{squashed}");
    let sha = text(&squashed, "sha").to_owned();
    let git = git2::Repository::open_bare(&path).unwrap();
    assert_eq!(
        git.find_commit(oid(&sha))
            .unwrap()
            .parent_id(0)
            .unwrap()
            .to_string(),
        before
    );
    assert_eq!(main_tip(&path), sha);
    assert_eq!(file_at(&path, &sha, "shared.txt"), b"B\n".to_vec());
    assert_eq!(app.alice.pull(2).await["state"], "merged");
    f.finish().await;
}

/// The top layer of a stack with open layers below it refuses squash and rebase, and its form offers
/// only the merge commit. The merge commit merges the whole stack.
async fn top_layer_merge_commit_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let root = tempfile::tempdir().unwrap();
    let app = setup(&mut f, root.path()).await;
    let main0 = app.alice.save("main", "README.md", "unborn").await;
    app.alice.branch("feature-1", &main0).await;
    let one = save_text(&app.alice, "feature-1", "one.txt", "1\n", &main0).await;
    app.alice.open_pull("One", "feature-1", "main").await;
    app.alice.branch("feature-2", &one).await;
    let two = save_text(&app.alice, "feature-2", "two.txt", "2\n", &one).await;
    app.alice.open_pull("Two", "feature-2", "feature-1").await;
    app.alice.branch("feature-3", &two).await;
    save_text(&app.alice, "feature-3", "three.txt", "3\n", &two).await;
    app.alice.open_pull("Three", "feature-3", "feature-2").await;
    join_stack(&app.alice, 2).await;
    join_stack(&app.alice, 3).await;

    for method in ["squash", "rebase"] {
        let (status, refused) = merge(&app.merger, 3, Some(method)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method}: {refused}");
        assert_eq!(
            refused["error"], "stack_needs_merge_commit",
            "{method}: {refused}"
        );
        assert_eq!(app.alice.pull(3).await["state"], "open");
    }
    let (status, page) = app.merger.page("/alice/proj/pulls/3").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("Squash and rebase are unavailable"),
        "the form for the top layer still offers squash or rebase"
    );
    let (status, merged) = merge(&app.merger, 3, Some("merge")).await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    for number in [1, 2, 3] {
        assert_eq!(app.alice.pull(number).await["state"], "merged");
    }
    f.finish().await;
}

#[tokio::test]
async fn sqlite_squash_merge_writes_one_commit_by_the_author() {
    squash_contract(false).await;
}

#[tokio::test]
async fn postgres_squash_merge_writes_one_commit_by_the_author() {
    squash_contract(true).await;
}

#[tokio::test]
async fn sqlite_rebase_merge_replays_each_commit_with_its_author() {
    rebase_contract(false).await;
}

#[tokio::test]
async fn postgres_rebase_merge_replays_each_commit_with_its_author() {
    rebase_contract(true).await;
}

#[tokio::test]
async fn sqlite_disallowed_merge_method_is_refused_and_not_offered() {
    disallowed_contract(false).await;
}

#[tokio::test]
async fn postgres_disallowed_merge_method_is_refused_and_not_offered() {
    disallowed_contract(true).await;
}

#[tokio::test]
async fn sqlite_rebase_refuses_a_merge_commit_in_the_head() {
    merge_commit_contract(false).await;
}

#[tokio::test]
async fn postgres_rebase_refuses_a_merge_commit_in_the_head() {
    merge_commit_contract(true).await;
}

#[tokio::test]
async fn sqlite_squash_and_rebase_conflicts_name_the_files() {
    conflict_contract(false).await;
}

#[tokio::test]
async fn postgres_squash_and_rebase_conflicts_name_the_files() {
    conflict_contract(true).await;
}

#[tokio::test]
async fn sqlite_native_merge_form_posts_each_method() {
    native_contract(false).await;
}

#[tokio::test]
async fn postgres_native_merge_form_posts_each_method() {
    native_contract(true).await;
}

#[tokio::test]
async fn sqlite_merge_without_a_method_is_a_merge_commit() {
    default_method_contract(false).await;
}

#[tokio::test]
async fn postgres_merge_without_a_method_is_a_merge_commit() {
    default_method_contract(true).await;
}

#[tokio::test]
async fn sqlite_settings_and_head_moves_are_judged_at_publish() {
    prepared_merge_contract(false).await;
}

#[tokio::test]
async fn postgres_settings_and_head_moves_are_judged_at_publish() {
    prepared_merge_contract(true).await;
}

#[tokio::test]
async fn sqlite_stacked_bottom_squash_moves_the_layer_above_to_a_rebase() {
    stacked_bottom_contract(false).await;
}

#[tokio::test]
async fn postgres_stacked_bottom_squash_moves_the_layer_above_to_a_rebase() {
    stacked_bottom_contract(true).await;
}

#[tokio::test]
async fn sqlite_branching_stack_squash_of_its_parent_moves_both_children() {
    branching_stack_contract(false).await;
}

#[tokio::test]
async fn postgres_branching_stack_squash_of_its_parent_moves_both_children() {
    branching_stack_contract(true).await;
}

#[tokio::test]
async fn sqlite_middle_layer_squash_then_rebase_stack_then_merge() {
    middle_layer_contract(false).await;
}

#[tokio::test]
async fn postgres_middle_layer_squash_then_rebase_stack_then_merge() {
    middle_layer_contract(true).await;
}

#[tokio::test]
async fn sqlite_refused_native_merge_keeps_the_submitted_method() {
    refused_form_contract(false).await;
}

#[tokio::test]
async fn postgres_refused_native_merge_keeps_the_submitted_method() {
    refused_form_contract(true).await;
}

#[tokio::test]
async fn sqlite_stacked_top_layer_still_squashes() {
    stacked_top_contract(false).await;
}

#[tokio::test]
async fn postgres_stacked_top_layer_still_squashes() {
    stacked_top_contract(true).await;
}

#[tokio::test]
async fn sqlite_top_layer_over_open_layers_merges_only_with_a_merge_commit() {
    top_layer_merge_commit_contract(false).await;
}

#[tokio::test]
async fn postgres_top_layer_over_open_layers_merges_only_with_a_merge_commit() {
    top_layer_merge_commit_contract(true).await;
}
