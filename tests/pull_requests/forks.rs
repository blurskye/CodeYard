//! Fork pull requests through JSON and native HTML, on both databases.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::http::StatusCode;
use base64::Engine;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::{queue::RunQueue, CiConfig, CiExecutionMode, CiRuns, CiWorkers},
    crypto::SecretEncryptor,
    db,
    repositories::{
        lifecycle::provision,
        pull_requests,
        store::{repo_store, FileWrite},
    },
    storage::ObjectStore,
};
use database::Database;
use pulls::{text, Client};
use serde_json::{json, Value};
use std::{collections::BTreeSet, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

async fn save(client: &Client, owner: &str, file: &str, previous: &str, content: &str) -> String {
    let value = client.expect("POST", &format!("/api/repos/{owner}/proj/contents/{file}"),
        json!({"content": base64::engine::general_purpose::STANDARD.encode(content), "message": format!("Add {file}"), "branch": "main", "expected_revision": previous}), StatusCode::CREATED).await;
    text(&value, "sha").to_owned()
}

/// Workflow paths admitted for one head commit, sorted.
async fn admitted_workflows(f: &Database, repo: i64, head: &str) -> Vec<String> {
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo, 100)
        .await
        .unwrap();
    let mut paths: Vec<String> = runs
        .into_iter()
        .filter(|run| run.head_sha.as_deref() == Some(head))
        .map(|run| run.workflow_path)
        .collect();
    paths.sort();
    paths
}

async fn contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let carol_id = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let base = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let encryptor = Arc::new(SecretEncryptor::new(&[42; 32]));
    db::secrets::create_or_update_secret(
        f.pool(),
        &encryptor,
        base.id,
        "BASE_SECRET",
        "must-not-be-delivered",
        Some(alice_id),
    )
    .await
    .unwrap();
    let runner = root.path().join("runner");
    let config = Arc::new(CiConfig {
        mode: CiExecutionMode::Local,
        act_binary: runner.to_str().unwrap().to_owned(),
        max_active_runs: 1,
        trusted_actor_ids: BTreeSet::from([alice_id, bob_id]),
        ..CiConfig::default()
    });
    let mut state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(encryptor),
        (*config).clone(),
        None,
    );
    let queued = Arc::new(tokio::sync::Notify::new());
    state.ci_runs = Arc::new(CiRuns::new(
        f.pool().clone(),
        config.clone(),
        queued.clone(),
        None,
        state.notification_broadcaster.clone(),
    ));
    let ci_runs = state.ci_runs.clone();
    let ci = CiWorkers::with_wakeup(
        RunQueue::Database {
            pool: f.pool().clone(),
            config: config.clone(),
            secrets: state.secret_encryptor.clone(),
            context: None,
            notifications: state.notification_broadcaster.clone(),
        },
        config,
        true,
        queued,
        root.path(),
    );
    let router = create_router(state);
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let carol = Client::login(&router, f.pool(), carol_id).await;
    let main0 = save(&alice, "alice", "README.md", "unborn", "base\n").await;
    let main1 = save(&alice, "alice", ".github/workflows/ci.yml", &main0,
        "name: ci\non: [pull_request]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ci\n").await;
    let fork = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/forks",
            Value::Null,
            StatusCode::CREATED,
        )
        .await;
    let fork_id = fork["repo"]["id"].as_i64().unwrap();
    let fork = db::repositories::records::get_repository_by_id(f.pool(), fork_id)
        .await
        .unwrap()
        .unwrap();
    let h1 = save(&bob, "bob", "fork.txt", &main1, "fork one\n").await;
    f.execute("WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<105) INSERT INTO repositories(owner_kind,owner_user_id,name,disk_path,is_fork,forked_from_id,created_at) SELECT 'user',$1,'paged_candidate_' || n,'/fixture/not-executed.git',TRUE,$2,'2099-01-01T00:00:00.000000Z' FROM numbers", &[alice_id, base.id]).await;
    let (status, form) = carol
        .page(&format!("/alice/proj/pulls/new?head_repo={fork_id}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{form}");
    assert!(
        form.contains("bob/proj"),
        "an explicit older head must remain reachable"
    );
    let (status, form) = bob.page("/alice/proj/pulls/new").await;
    assert_eq!(status, StatusCode::OK, "{form}");
    assert!(
        form.contains("Head repository") && form.contains("bob/proj"),
        "{form}"
    );
    // The author's own fork stays in the choices when another head is selected.
    let (status, form) = bob
        .page(&format!("/alice/proj/pulls/new?head_repo={}", base.id))
        .await;
    assert_eq!(status, StatusCode::OK, "{form}");
    assert!(
        form.contains("bob/proj"),
        "the author's own fork must remain beyond the first page of forks"
    );
    // Native creation defaults to the author's own fork and permits main -> main.
    let (status, page) = bob
        .form(
            "/alice/proj/pulls/new",
            &[
                ("title", "From fork"),
                ("head", "main"),
                ("base", "main"),
                ("intent", "create"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let opened = bob.pull(1).await;
    assert_eq!(opened["head_repo_id"], fork_id);
    assert_eq!(opened["head_sha"], h1);
    f.execute(
        "DELETE FROM repositories WHERE forked_from_id=$1 AND name LIKE 'paged_candidate_%'",
        &[base.id],
    )
    .await;
    let (status, page) = alice.page("/alice/proj/pulls/1").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page.contains("bob/proj:main") && page.contains("Run workflows"),
        "{page}"
    );
    assert!(bob
        .page("/alice/proj/pulls")
        .await
        .1
        .contains("bob/proj:main"));
    let diff = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls/1/diff",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert!(
        diff.to_string().contains("fork.txt") && diff.to_string().contains("fork one"),
        "{diff}"
    );
    assert!(db::ci::list_ci_runs_for_repo(f.pool(), base.id, 20)
        .await
        .unwrap()
        .is_empty());
    bob.expect(
        "POST",
        "/api/repos/alice/proj/pulls/1/workflows",
        json!({"expected_head": h1}),
        StatusCode::NOT_FOUND,
    )
    .await;
    // General CI ingress cannot bypass the pull request's approval.
    alice.expect("POST", "/api/ci/trigger", json!({"repo_owner": "alice", "repo_name": "proj", "branch": "main", "event": "pull_request", "head_sha": h1, "pipeline": "ci"}), StatusCode::FORBIDDEN).await;
    let (status, page) = alice
        .form("/alice/proj/pulls/1/workflows", &[("expected_head", &h1)])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), base.id, 20)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].pull_request_id, opened["id"].as_i64());
    assert!(runs[0].is_fork);
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/workflows",
            json!({"expected_head": h1}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        db::ci::list_ci_runs_for_repo(f.pool(), base.id, 20)
            .await
            .unwrap()
            .len(),
        1,
        "one run per commit"
    );
    alice.expect("PUT", "/api/repos/alice/proj/branch-protection", json!({"pattern": "main", "require_pull_request": true, "required_approving_reviews": 1}), StatusCode::OK).await;
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/reviews",
            json!({"state": "approved", "commit_sha": h1}),
            StatusCode::CREATED,
        )
        .await;
    // A refresh interrupted after exact ref publication but before the row update
    // is recovered idempotently by the next refresh.
    let base_git = git2::Repository::open_bare(&base.disk_path).unwrap();
    let h2 = repo_store(&fork, None)
        .save_file_as(
            f.pool(),
            fork.id,
            bob_id,
            &FileWrite {
                branch: "main".into(),
                path: "fork-two.txt".into(),
                message: "Add fork-two.txt".into(),
                expected_revision: h1.clone(),
                creating: true,
                deleting: false,
            },
            b"fork two\n",
            "Bob",
            "bob@example.invalid",
        )
        .await
        .unwrap()
        .sha;
    repo_store(&base, None)
        .fetch_pull_head(
            f.pool(),
            base.id,
            fork.id,
            bob_id,
            &repo_store(&fork, None),
            1,
            &h2,
            Some(&h1),
        )
        .await
        .unwrap();
    pull_requests::refresh_open_pull_requests(
        f.pool(),
        &fork,
        None,
        &ci_runs,
        bob_id,
        codeyard::backend::db::models::AccessLevel::Owner,
    )
    .await
    .unwrap();
    let moved = bob.pull(1).await;
    assert_eq!(moved["head_sha"], h2);
    assert!(pulls::moved_forward(&opened, &moved));
    assert_eq!(
        base_git
            .find_reference("refs/pull/1/head")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        h2
    );
    assert_eq!(
        db::ci::list_ci_runs_for_repo(f.pool(), base.id, 20)
            .await
            .unwrap()
            .len(),
        1,
        "new fork commit waits for approval"
    );
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/workflows",
            json!({"expected_head": h1}),
            StatusCode::CONFLICT,
        )
        .await;
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/workflows",
            json!({"expected_head": h2}),
            StatusCode::OK,
        )
        .await;
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), base.id, 20)
        .await
        .unwrap();
    assert_eq!(runs.len(), 2);

    // Both runs must fetch only their admitted history, even from a pack that
    // also holds a large unrelated branch. Loose unreachable objects stay behind too.
    let unrelated = {
        let mut tree = base_git.treebuilder(None).unwrap();
        tree.insert(
            "large.bin",
            base_git.blob(&vec![b'x'; 1024 * 1024]).unwrap(),
            0o100644,
        )
        .unwrap();
        let tree = base_git.find_tree(tree.write().unwrap()).unwrap();
        let author = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        base_git
            .commit(
                Some("refs/heads/unrelated"),
                &author,
                &author,
                "unrelated",
                &tree,
                &[],
            )
            .unwrap()
    };
    base_git
        .tag_lightweight(
            "unrelated-tag",
            &base_git.find_object(unrelated, None).unwrap(),
            false,
        )
        .unwrap();
    let repack = std::process::Command::new("git")
        .args(["--git-dir", &base.disk_path, "repack", "-ad"])
        .output()
        .unwrap();
    assert!(
        repack.status.success(),
        "{}",
        String::from_utf8_lossy(&repack.stderr)
    );
    let unreachable = base_git.blob(b"unreachable checkout sentinel").unwrap();
    base_git
        .config()
        .unwrap()
        .set_str("ci.sourceOnly", "must-not-be-inherited")
        .unwrap();
    let hook = base_git.path().join("hooks/post-checkout");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\nexit 43\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();

    // The production worker invokes this executable instead of act. Check the
    // actual job checkout and keep the fork's secret-file refusal.
    std::fs::write(
        &runner,
        format!(
            r#"#!/bin/sh
set -eu
checkout=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --secret-file) exit 42 ;;
        --directory) shift; checkout=$1 ;;
    esac
    shift
done
test -n "$checkout"
cd "$checkout"
test "$(git rev-parse HEAD)" = "$GITSERV_HEAD_SHA"
test -z "$(git symbolic-ref -q HEAD || :)"
git merge-base --is-ancestor '{main0}' HEAD
test "$(git show HEAD:fork.txt)" = 'fork one'
test "$(cat fork.txt)" = 'fork one'
test "$(git config --get remote.origin.url)" = '{source}'
test "$(git config --get remote.origin.tagOpt)" = '--no-tags'
test -z "$(git config --get ci.sourceOnly || :)"
test ! -e .git/hooks/post-checkout
test ! -e .git/objects/info/alternates
test ! -e .git/objects/info/http-alternates
test ! -e .git/shallow
test -z "$(find .git/objects \( -type l -o -type f -links +1 \) -print)"
git rev-list --objects HEAD | cut -d' ' -f1 | sort > ../reachable
git cat-file --batch-all-objects --batch-check='%(objectname)' | sort > ../objects
cmp ../reachable ../objects
echo fork-workflow-without-secrets
echo isolated-checkout-only-admitted-objects
"#,
            source = base.disk_path
        ),
    )
    .unwrap();
    std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    ci.startup().await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        for run in &runs {
            loop {
                let job = alice
                    .expect(
                        "GET",
                        &format!("/api/ci/jobs/{}", run.run_id),
                        Value::Null,
                        StatusCode::OK,
                    )
                    .await;
                let status = job["status"].as_str().unwrap();
                if status == "success" {
                    break;
                }
                assert!(status == "queued" || status == "running", "{job}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let (_, logs) = alice
                .send(
                    "GET",
                    &format!("/api/ci/jobs/{}/logs", run.run_id),
                    "application/json",
                    String::new(),
                )
                .await;
            assert!(logs.contains("fork-workflow-without-secrets"), "{logs}");
            assert!(
                logs.contains("isolated-checkout-only-admitted-objects"),
                "{logs}"
            );
            assert!(!logs.contains("must-not-be-delivered"));
        }
    })
    .await
    .expect("fork workflows completed");
    ci.shutdown().await.unwrap();
    assert!(base_git.odb().unwrap().exists(unrelated));
    assert!(base_git.odb().unwrap().exists(unreachable));
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/merge",
            json!({"expected_head": h2, "expected_base": main1}),
            StatusCode::CONFLICT,
        )
        .await;
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/reviews",
            json!({"state": "approved", "commit_sha": h2}),
            StatusCode::CREATED,
        )
        .await;
    let merged = alice.merge_current(1).await;
    assert_eq!(
        base_git
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        text(&merged, "sha")
    );
    assert!(repo_store(&base, None)
        .blob("main", "fork-two.txt")
        .await
        .unwrap()
        .content
        .contains("fork two"));

    // An unreadable fork is refused, and an unrelated repository is not a head.
    let private_fork = carol
        .expect(
            "POST",
            "/api/repos/alice/proj/forks",
            Value::Null,
            StatusCode::CREATED,
        )
        .await;
    let private_fork_id = private_fork["repo"]["id"].as_i64().unwrap();
    f.execute(
        "UPDATE repositories SET is_private=TRUE WHERE id=$1",
        &[private_fork_id],
    )
    .await;
    bob.expect("POST", "/api/repos/alice/proj/pulls", json!({"title": "Hidden head", "head_repo_id": private_fork_id, "head_branch": "main", "base_branch": "main"}), StatusCode::NOT_FOUND).await;
    let private = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "private",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    carol
        .expect(
            "GET",
            "/api/repos/alice/private/pulls",
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    carol
        .expect(
            "POST",
            "/api/repos/alice/private/pulls",
            json!({"title": "Invisible", "head_branch": "main", "base_branch": "main"}),
            StatusCode::NOT_FOUND,
        )
        .await;
    assert_eq!(
        carol.page("/alice/private/pulls").await.0,
        StatusCode::NOT_FOUND
    );
    alice.expect("POST", "/api/repos/alice/proj/pulls", json!({"title": "Unrelated", "head_repo_id": private.id, "head_branch": "main", "base_branch": "main"}), StatusCode::BAD_REQUEST).await;

    // The fork can disappear after opening; its retained ref still merges, and
    // the native form does not assume the head is a base-repository branch.
    let h3 = save(&bob, "bob", "last.txt", &h2, "retained\n").await;
    bob.expect("POST", "/api/repos/alice/proj/pulls", json!({"title": "Retain deleted fork", "head_repo_id": fork_id, "head_branch": "main", "base_branch": "main"}), StatusCode::CREATED).await;
    bob.expect(
        "DELETE",
        "/api/repos/bob/proj",
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let deleted = alice.pull(2).await;
    assert!(deleted["head_repo_id"].is_null());
    assert_eq!(deleted["head_sha"], h3);
    let (status, page) = alice.page("/alice/proj/pulls/2").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("deleted repository"), "{page}");
    assert!(alice
        .page("/alice/proj/pulls")
        .await
        .1
        .contains("deleted repository"));
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/2/reviews",
            json!({"state": "approved", "commit_sha": h3}),
            StatusCode::CREATED,
        )
        .await;
    // The review advanced the form revision; submit the freshly read values.
    let deleted = alice.pull(2).await;
    let (status, page) = alice
        .form(
            "/alice/proj/pulls/2/merge",
            &[
                ("expected_state", "open"),
                ("expected_updated", text(&deleted, "updated_at")),
                ("expected_head", &h3),
                ("expected_base", text(&deleted, "base_sha")),
                ("confirm", "merge"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    assert_eq!(alice.pull(2).await["state"], "merged");
    assert!(repo_store(&base, None)
        .blob("main", "last.txt")
        .await
        .unwrap()
        .content
        .contains("retained"));

    // Same-repository pull requests automatically queue every new head once.
    let tip = repo_store(&base, None).commit("main").await.unwrap().sha;
    alice.branch("topic", &tip).await;
    let t1 = alice.save("topic", "topic.txt", &tip).await;
    let same_pull = alice.open_pull("Same repository", "topic", "main").await;
    let t2 = alice.save("topic", "topic-two.txt", &t1).await;
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), base.id, 20)
        .await
        .unwrap();
    for sha in [&t1, &t2] {
        let matching = runs
            .iter()
            .filter(|run| run.head_sha.as_ref() == Some(sha))
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].pull_request_id, same_pull["id"].as_i64());
        assert!(!matching[0].is_fork);
    }
    // A head save must retain both events after integrating push workflow queuing.
    let workflow = base64::engine::general_purpose::STANDARD.encode("name: ci\non: [push, pull_request]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo both events\n");
    let saved = alice.expect("PUT", "/api/repos/alice/proj/contents/.github/workflows/ci.yml",
        json!({"content": workflow, "message": "Run both events", "branch": "topic", "expected_revision": t2}), StatusCode::OK).await;
    let t3 = text(&saved["commit"], "sha");
    assert_eq!(alice.pull(3).await["head_sha"], t3);
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), base.id, 20)
        .await
        .unwrap();
    for event in ["push", "pull_request"] {
        let matching = runs
            .iter()
            .filter(|run| run.head_sha.as_deref() == Some(t3) && run.event == event)
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "{event} remains queued for the new head");
        assert!(!matching[0].is_fork);
    }
    let malformed = base64::engine::general_purpose::STANDARD.encode("name: broken\non: [push\n");
    let saved = alice.expect("POST", "/api/repos/alice/proj/contents/.github/workflows/broken.yml",
        json!({"content": malformed, "message": "Add malformed workflow", "branch": "topic", "expected_revision": t3}), StatusCode::CREATED).await;
    let t4 = text(&saved, "sha");
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), base.id, 30)
        .await
        .unwrap();
    for event in ["push", "pull_request"] {
        let matching = runs
            .iter()
            .filter(|run| run.head_sha.as_deref() == Some(t4) && run.event == event)
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            1,
            "a malformed neighbor cannot suppress {event}"
        );
        assert_eq!(matching[0].workflow_path, ".github/workflows/ci.yml");
    }

    // A trusted writer may approve fork workflows, but a deployment workflow also needs
    // deployment rights. The refusal must be reported, the workflow that was admitted
    // must not be queued again, and someone who may deploy must still complete the head.
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob_id,
        base.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    // The fork above was deleted, so the deployment head lives on a new fork.
    let new_fork = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/forks",
            Value::Null,
            StatusCode::CREATED,
        )
        .await;
    let new_fork_id = new_fork["repo"]["id"].as_i64().unwrap();
    let new_fork = db::repositories::records::get_repository_by_id(f.pool(), new_fork_id)
        .await
        .unwrap()
        .unwrap();
    let tip = repo_store(&new_fork, None)
        .commit("main")
        .await
        .unwrap()
        .sha;
    let deploy_head = save(
        &bob,
        "bob",
        ".github/workflows/deploy.yml",
        &tip,
        "name: deploy\non: [pull_request]\njobs:\n  deploy:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo deploy\n",
    )
    .await;
    let deploy_pull = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls",
            json!({"title": "Deploy workflow", "head_repo_id": new_fork_id, "head_branch": "main", "base_branch": "main"}),
            StatusCode::CREATED,
        )
        .await;
    let number = deploy_pull["number"].as_i64().unwrap();
    let approve = format!("/api/repos/alice/proj/pulls/{number}/workflows");
    let (status, refused) = bob
        .api("POST", &approve, json!({"expected_head": deploy_head}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"], "ci_not_queued", "{refused}");
    assert!(
        text(&refused, "message").contains("CI deployment permission denied"),
        "{refused}"
    );
    assert_eq!(
        admitted_workflows(&f, base.id, &deploy_head).await,
        [".github/workflows/ci.yml"]
    );
    let (status, page) = bob
        .form(
            &format!("/alice/proj/pulls/{number}/workflows"),
            &[("expected_head", &deploy_head)],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    assert!(
        page.contains("CI deployment permission denied") && page.contains("Run workflows"),
        "{page}"
    );
    let both = [".github/workflows/ci.yml", ".github/workflows/deploy.yml"];
    for _ in 0..2 {
        alice
            .expect(
                "POST",
                &approve,
                json!({"expected_head": deploy_head}),
                StatusCode::OK,
            )
            .await;
        assert_eq!(admitted_workflows(&f, base.id, &deploy_head).await, both);
    }
    drop(base_git);
    f.finish().await;
}

async fn private_preview_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let base = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let anonymous = Client::anonymous(&router);
    let common = save(&alice, "alice", "README.md", "unborn", "common\n").await;
    let fork = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/forks",
            Value::Null,
            StatusCode::CREATED,
        )
        .await;
    let fork_id = fork["repo"]["id"].as_i64().unwrap();
    f.execute(
        "UPDATE repositories SET is_private=TRUE WHERE id=$1",
        &[fork_id],
    )
    .await;
    let fork = db::repositories::records::get_repository_by_id(f.pool(), fork_id)
        .await
        .unwrap()
        .unwrap();
    let public_sha = save(&alice, "alice", "public.txt", &common, "public advance\n").await;
    let private_sha = save(
        &bob,
        "bob",
        "secret.txt",
        &common,
        "private-preview-marker\n",
    )
    .await;
    let base_store = repo_store(&base, None);
    let fork_store = repo_store(&fork, None);
    let public_path = format!("/api/repos/alice/proj/commits/{public_sha}");
    let private_path = format!("/api/repos/alice/proj/commits/{private_sha}");
    let base_refs = base_store.refs().await.unwrap();
    let fork_refs = fork_store.refs().await.unwrap();
    for reader in [&alice, &anonymous] {
        reader
            .expect("GET", &public_path, Value::Null, StatusCode::OK)
            .await;
        // A commit that is not in the repository is a missing resource.
        reader
            .expect("GET", &private_path, Value::Null, StatusCode::NOT_FOUND)
            .await;
    }
    assert_eq!(
        base_store.commit(&private_sha).await.unwrap_err().kind,
        codeyard::backend::repositories::store::ErrorKind::NotFound
    );
    assert_eq!(
        fork_store.commit(&public_sha).await.unwrap_err().kind,
        codeyard::backend::repositories::store::ErrorKind::NotFound
    );
    // The diff waits for the compare marker, so the page is asked to compare.
    let uri =
        format!("/alice/proj/pulls/new?head_repo={fork_id}&head=main&base=main&intent=compare");
    let (status, html) = bob.page(&uri).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("private-preview-marker"));
    let (status, html) = bob
        .form(
            "/alice/proj/pulls/new",
            &[
                ("head_repo", &fork_id.to_string()),
                ("head", "main"),
                ("base", "main"),
                ("intent", "compare"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("private-preview-marker"));
    {
        use axum::{
            body::{to_bytes, Body},
            http::Request,
        };
        use tower::ServiceExt;
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&uri)
                    .header("Cookie", format!("session_id={}", bob.session))
                    .header("Accept", "application/vnd.codeyard.page+json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        let envelope: codeyard::webui::app::PageEnvelope = serde_json::from_slice(&bytes).unwrap();
        let codeyard::webui::app::Page::Collaboration(page) = envelope.page else {
            panic!("expected collaboration model")
        };
        let codeyard::webui::pages::repos::issues::Page::New {
            diff: Some(diff), ..
        } = *page
        else {
            panic!("expected comparison model")
        };
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].filename, "secret.txt");
        assert!(diff.files[0].patch.contains("+private-preview-marker\n"));
    }
    assert_eq!(
        base_store.commit(&private_sha).await.unwrap_err().kind,
        codeyard::backend::repositories::store::ErrorKind::NotFound
    );
    assert_eq!(
        fork_store.commit(&public_sha).await.unwrap_err().kind,
        codeyard::backend::repositories::store::ErrorKind::NotFound
    );
    assert_eq!(base_store.refs().await.unwrap(), base_refs);
    assert_eq!(fork_store.refs().await.unwrap(), fork_refs);
    for reader in [&alice, &anonymous] {
        reader
            .expect("GET", &public_path, Value::Null, StatusCode::OK)
            .await;
        reader
            .expect("GET", &private_path, Value::Null, StatusCode::NOT_FOUND)
            .await;
    }
    assert!(alice
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls",
            Value::Null,
            StatusCode::OK
        )
        .await
        .as_array()
        .unwrap()
        .is_empty());
    let opened = bob.expect("POST", "/api/repos/alice/proj/pulls", json!({
        "title": "Explicitly shared", "head_repo_id": fork_id, "head_branch": "main", "base_branch": "main",
    }), StatusCode::CREATED).await;
    assert_eq!(opened["head_sha"], private_sha);
    for reader in [&alice, &anonymous] {
        reader
            .expect("GET", &private_path, Value::Null, StatusCode::OK)
            .await;
    }
    alice.merge_current(1).await;
    assert_eq!(
        base_store.blob("main", "secret.txt").await.unwrap().content,
        "private-preview-marker\n"
    );
    assert_eq!(
        base_store.blob("main", "public.txt").await.unwrap().content,
        "public advance\n"
    );
    f.finish().await;
}

async fn private_fork_revocation_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("revocation_alice").await;
    let bob_id = f.actor("revocation_bob").await;
    let carol_id = f.actor("revocation_carol").await;
    let root = tempfile::tempdir().unwrap();
    let base = provision(
        (f.pool(), None),
        root.path(),
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
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let ci = state.ci_runs.clone();
    let router = create_router(state);
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let carol = Client::login(&router, f.pool(), carol_id).await;
    let base_head = save(&alice, "revocation_alice", "README.md", "unborn", "base\n").await;
    let fork = carol
        .expect(
            "POST",
            "/api/repos/revocation_alice/proj/forks",
            Value::Null,
            StatusCode::CREATED,
        )
        .await;
    let fork_id = fork["repo"]["id"].as_i64().unwrap();
    f.execute(
        "UPDATE repositories SET is_private=TRUE WHERE id=$1",
        &[fork_id],
    )
    .await;
    let fork = db::repositories::records::get_repository_by_id(f.pool(), fork_id)
        .await
        .unwrap()
        .unwrap();
    let first = save(
        &carol,
        "revocation_carol",
        "private-one.txt",
        &base_head,
        "one\n",
    )
    .await;
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob_id,
        fork.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let opened = bob
        .expect(
            "POST",
            "/api/repos/revocation_alice/proj/pulls",
            json!({
                "title": "Private fork",
                "head_repo_id": fork.id,
                "head_branch": "main",
                "base_branch": "main"
            }),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(opened["head_sha"], first);
    let base_git = git2::Repository::open_bare(&base.disk_path).unwrap();
    assert_eq!(
        base_git
            .refname_to_id("refs/pull/1/head")
            .unwrap()
            .to_string(),
        first
    );

    alice
        .expect(
            "POST",
            "/api/repos/revocation_alice/proj/branches",
            json!({"name": "later", "sha": base_head}),
            StatusCode::OK,
        )
        .await;
    let later_first = repo_store(&base, None)
        .save_file_as(
            f.pool(),
            base.id,
            alice_id,
            &FileWrite {
                branch: "later".into(),
                path: "later-one.txt".into(),
                message: "start later pull request".into(),
                expected_revision: base_head.clone(),
                creating: true,
                deleting: false,
            },
            b"later one\n",
            "Alice",
            "alice@example.invalid",
        )
        .await
        .unwrap();
    let later = alice
        .expect(
            "POST",
            "/api/repos/revocation_alice/proj/pulls",
            json!({"title": "Later pull", "head_branch": "later", "base_branch": "main"}),
            StatusCode::CREATED,
        )
        .await;

    assert!(
        db::repositories::access::revoke_repo_access(f.pool(), bob_id, fork.id)
            .await
            .unwrap()
    );
    let second = repo_store(&fork, None)
        .save_file_as(
            f.pool(),
            fork.id,
            carol_id,
            &FileWrite {
                branch: "main".into(),
                path: "private-two.txt".into(),
                message: "advance private fork".into(),
                expected_revision: first.clone(),
                creating: true,
                deleting: false,
            },
            b"two\n",
            "Carol",
            "carol@example.invalid",
        )
        .await
        .unwrap();
    let advanced_base = repo_store(&base, None)
        .save_file_as(
            f.pool(),
            base.id,
            alice_id,
            &FileWrite {
                branch: "main".into(),
                path: "base-two.txt".into(),
                message: "advance base".into(),
                expected_revision: base_head.clone(),
                creating: true,
                deleting: false,
            },
            b"base two\n",
            "Alice",
            "alice@example.invalid",
        )
        .await
        .unwrap();
    let later_second = repo_store(&base, None)
        .save_file_as(
            f.pool(),
            base.id,
            alice_id,
            &FileWrite {
                branch: "later".into(),
                path: "later-two.txt".into(),
                message: "advance later pull request".into(),
                expected_revision: later_first.sha.clone(),
                creating: true,
                deleting: false,
            },
            b"later two\n",
            "Alice",
            "alice@example.invalid",
        )
        .await
        .unwrap();
    assert!(pull_requests::refresh_open_pull_requests(
        f.pool(),
        &base,
        Some(&["main", "later"][..]),
        &ci,
        alice_id,
        codeyard::backend::db::models::AccessLevel::Owner
    )
    .await
    .is_err());
    let stored = db::pull_requests::records::get_pull_request_by_id(
        f.pool(),
        opened["id"].as_i64().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(stored.head_sha, first);
    assert_eq!(stored.base_sha, advanced_base.sha);
    let later_stored =
        db::pull_requests::records::get_pull_request_by_id(f.pool(), later["id"].as_i64().unwrap())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(later_stored.head_sha, later_second.sha);
    assert_eq!(later_stored.base_sha, advanced_base.sha);
    assert_eq!(
        base_git
            .refname_to_id("refs/pull/1/head")
            .unwrap()
            .to_string(),
        first
    );
    assert!(git2::Repository::open_bare(&base.disk_path)
        .unwrap()
        .find_object(git2::Oid::from_str(&second.sha).unwrap(), None)
        .is_err());
    assert_eq!(
        repo_store(&fork, None).commit("main").await.unwrap().sha,
        second.sha
    );

    // Simulate interruption after the PR row commits but before its initial pull
    // ref is published. Repeating the create request repairs only the absent ref
    // with an exact create CAS, then reports the existing PR.
    repo_store(&fork, None)
        .create_branch_as(f.pool(), fork.id, carol_id, "retry", &second.sha)
        .await
        .unwrap();
    let interrupted = db::pull_requests::records::create_pull_request(
        f.pool(),
        db::pull_requests::records::NewPullRequest {
            repo_id: base.id,
            head_repo_id: Some(fork.id),
            author_id: carol_id,
            title: "Interrupted publication",
            body: "",
            head_branch: "retry",
            head_sha: &second.sha,
            base_branch: "main",
            base_sha: &advanced_base.sha,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let pull_ref = format!("refs/pull/{}/head", interrupted.number);
    assert!(base_git.find_reference(&pull_ref).is_err());
    base_git
        .reference(
            &pull_ref,
            git2::Oid::from_str(&advanced_base.sha).unwrap(),
            true,
            "simulate unrelated initial publication",
        )
        .unwrap();
    let retry_body = json!({
        "title": "Retry interrupted publication",
        "head_repo_id": fork.id,
        "head_branch": "retry",
        "base_branch": "main"
    });
    let (status, stale) = carol
        .api(
            "POST",
            "/api/repos/revocation_alice/proj/pulls",
            retry_body.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{stale}");
    assert_eq!(
        base_git.refname_to_id(&pull_ref).unwrap().to_string(),
        advanced_base.sha
    );
    base_git
        .find_reference(&pull_ref)
        .unwrap()
        .delete()
        .unwrap();
    let (status, duplicate) = carol
        .api("POST", "/api/repos/revocation_alice/proj/pulls", retry_body)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{duplicate}");
    assert_eq!(duplicate["error"], "pull_request_exists");
    assert_eq!(
        base_git.refname_to_id(&pull_ref).unwrap().to_string(),
        second.sha
    );

    // The revoked private fork is unrelated to this healthy PR and must not
    // prevent its own target-only refresh and merge.
    let merged = alice
        .expect(
            "POST",
            &format!(
                "/api/repos/revocation_alice/proj/pulls/{}/merge",
                later["number"].as_i64().unwrap()
            ),
            json!({
                "expected_head": later_stored.head_sha,
                "expected_base": later_stored.base_sha,
            }),
            StatusCode::OK,
        )
        .await;
    assert_eq!(merged["merged"], true);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_private_fork_revocation_blocks_refresh_publication() {
    private_fork_revocation_contract(false).await;
}

#[tokio::test]
async fn postgres_private_fork_revocation_blocks_refresh_publication() {
    private_fork_revocation_contract(true).await;
}

#[tokio::test]
async fn sqlite_private_fork_previews_do_not_publish_objects() {
    private_preview_contract(false).await;
}
#[tokio::test]
async fn postgres_private_fork_previews_do_not_publish_objects() {
    private_preview_contract(true).await;
}

#[tokio::test]
async fn sqlite_fork_pull_requests() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_fork_pull_requests() {
    contract(true).await;
}

/// A public pull request from a private fork. The fork's name and link show only to
/// people who can read the fork, also after the fork is renamed. The commits and the
/// diff stay readable to anyone who can read the base repository.
async fn private_head_rename_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let anonymous = Client::anonymous(&router);
    let main = save(&alice, "alice", "README.md", "unborn", "base\n").await;
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
        "PATCH",
        "/api/repos/bob/proj",
        json!({"is_private": true}),
        StatusCode::OK,
    )
    .await;
    let head = save(&bob, "bob", "fork.txt", &main, "fork one\n").await;
    let opened = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls",
            json!({"title": "From private fork", "head_repo_id": fork_id, "head_branch": "main", "base_branch": "main"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(opened["head_sha"], head);
    let (status, page) = anonymous.page("/alice/proj/pulls/1").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page.contains("unavailable repository") && !page.contains("bob/proj:main"),
        "{page}"
    );
    bob.expect(
        "POST",
        "/api/repos/bob/proj/rename",
        json!({"new_name": "renamed"}),
        StatusCode::OK,
    )
    .await;
    for reader in [&anonymous, &alice] {
        let (status, page) = reader.page("/alice/proj/pulls/1").await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(
            page.contains("unavailable repository") && !page.contains("renamed"),
            "{page}"
        );
        let (status, list) = reader.page("/alice/proj/pulls").await;
        assert_eq!(status, StatusCode::OK, "{list}");
        assert!(
            list.contains("unavailable repository") && !list.contains("renamed"),
            "{list}"
        );
    }
    let (status, page) = bob.page("/alice/proj/pulls/1").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("bob/renamed:main"), "{page}");
    assert!(bob
        .page("/alice/proj/pulls")
        .await
        .1
        .contains("bob/renamed:main"));
    anonymous
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/commits/{head}"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    anonymous
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls/1/diff",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    f.finish().await;
}

#[tokio::test]
async fn sqlite_private_head_rename_hides_name_from_readers() {
    private_head_rename_contract(false).await;
}
#[tokio::test]
async fn postgres_private_head_rename_hides_name_from_readers() {
    private_head_rename_contract(true).await;
}
