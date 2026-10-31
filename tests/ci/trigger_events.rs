//! Pull request activity filters and base-workflow authority through the HTTP API.
use crate::support::{database::Database, fake_webui_bundle, http_client::Client};
use axum::http::StatusCode;
use base64::{engine::general_purpose::STANDARD, Engine};
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::{queue::RunQueue, CiConfig, CiExecutionMode},
    crypto::SecretEncryptor,
    db,
    repositories::lifecycle::provision,
    storage::ObjectStore,
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc};
use tower::ServiceExt;

async fn save(client: &Client, owner: &str, path: &str, previous: &str, content: &str) -> String {
    let value = client
        .expect(
            "POST",
            &format!("/api/repos/{owner}/proj/contents/{path}"),
            json!({"content": STANDARD.encode(content), "message": "Update fixture", "branch": "main", "expected_revision": previous}),
            StatusCode::CREATED,
        )
        .await;
    value["sha"].as_str().unwrap().to_owned()
}

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    // Host execution still requires an explicitly trusted actor and repository write access.
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let encryptor = Arc::new(SecretEncryptor::new(&[42; 32]));
    db::secrets::create_or_update_secret(
        f.pool(),
        &encryptor,
        repo.id,
        "BASE_SECRET",
        "base-value",
        Some(alice),
        &db::users::accounts::AccountCredential::Session(&crate::support::live_session(f.pool(), alice).await),
    )
    .await
    .unwrap();
    let config = Arc::new(CiConfig {
        trusted_actor_ids: BTreeSet::from([alice, bob]),
        ..Default::default()
    });
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(encryptor.clone()),
        (*config).clone(),
        None,
    );
    let queue = RunQueue::Database {
        pool: f.pool().clone(),
        config: config.clone(),
        secrets: Some(encryptor),
        context: None,
        notifications: state.notification_broadcaster.clone(),
    };
    let router = create_router(state);
    let owner = Client::login(&router, f.pool(), alice).await;
    let contributor = Client::login(&router, f.pool(), bob).await;
    let start = save(&owner, "alice", "README.md", "unborn", "base\n").await;
    let default = "name: default\non: pull_request\nconcurrency: fork-${{ github.ref }}\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{run: echo default}]\n";
    let edited = "name: edited\non:\n  pull_request:\n    types: [edited, closed]\n    branches: [main]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{run: echo edited}]\n";
    let target = "name: target\nconcurrency: target-${{ github.event.action }}-${{ github.ref }}\non:\n  pull_request_target:\n    branches: [main]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{run: echo trusted-base}]\n";
    let base = save(
        &owner,
        "alice",
        ".github/workflows/default.yml",
        &start,
        default,
    )
    .await;
    let base = save(
        &owner,
        "alice",
        ".github/workflows/edited.yml",
        &base,
        edited,
    )
    .await;
    let base = save(
        &owner,
        "alice",
        ".github/workflows/target.yml",
        &base,
        target,
    )
    .await;
    contributor
        .expect(
            "POST",
            "/api/repos/alice/proj/forks",
            Value::Null,
            StatusCode::CREATED,
        )
        .await;
    let changed = contributor
        .expect(
            "PUT",
            "/api/repos/bob/proj/contents/.github/workflows/target.yml",
            json!({"content": STANDARD.encode(target.replace("trusted-base", "untrusted-head")), "message": "Change head workflow", "branch": "main", "expected_revision": base}),
            StatusCode::OK,
        )
        .await;
    let head = changed["commit"]["sha"].as_str().unwrap().to_owned();
    let opened = contributor.expect("POST", "/api/repos/alice/proj/pulls", json!({"title":"Fork activity", "head_repo_id": db::repositories::records::get_repository(f.pool(), "bob", "proj").await.unwrap().unwrap().id, "head_branch":"main", "base_branch":"main"}), StatusCode::CREATED).await;
    let pull = db::pull_requests::records::get_pull_request(f.pool(), repo.id, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(opened["head_sha"], head);
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
        .await
        .unwrap();
    assert_eq!(runs.len(), 2, "{runs:?}");
    let waiting = runs.iter().find(|run| run.event == "pull_request").unwrap();
    assert!(waiting.is_fork && waiting.approval_required);
    assert!(
        db::action_concurrency::groups(f.pool(), repo.id, Some(waiting.id))
            .await
            .unwrap()
            .is_empty()
    );
    let run = runs
        .iter()
        .find(|run| run.event == "pull_request_target")
        .unwrap();
    let groups = db::action_concurrency::groups(f.pool(), repo.id, Some(run.id))
        .await
        .unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].name, "target-opened-refs/heads/main");
    assert_eq!(run.event, "pull_request_target");
    assert_eq!(run.branch, "main");
    assert_eq!(run.head_sha.as_deref(), Some(base.as_str()));
    assert!(!run.is_fork);
    assert_eq!(run.pull_request_id, Some(pull.id));
    let payload = run.event_payload.as_ref().unwrap();
    assert_eq!(payload["action"], "opened");
    assert_eq!(payload["pull_request"]["head"]["sha"], head);
    assert_eq!(payload["pull_request"]["base"]["sha"], base);
    assert_eq!(payload["sender"]["id"], bob);
    let claim = queue
        .claim("target-worker", CiExecutionMode::Local, &codeyard::backend::ci::CiConfig::default().platform_labels())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.run.id, run.id);
    assert_eq!(claim.secrets, [("BASE_SECRET".into(), "base-value".into())]);
    let credentials = claim.github_tokens.workflows.values().next().unwrap();
    let token = credentials.values().next().unwrap();
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v3/repos/alice/proj/git/blobs")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"content":"base-write"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    super::actions_workflows::checked(
        &router,
        "pulls/update",
        "PATCH",
        "/api/v3/repos/alice/proj/pulls/1",
        token,
        json!({"title":"Edited by a job"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
            .await
            .unwrap()
            .len(),
        2
    );
    // Approval starts the fork workflow, but never grants the head the base's secrets.
    owner
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/workflows",
            json!({"expected_head":head}),
            StatusCode::OK,
        )
        .await;
    let claim = queue
        .claim("fork-worker", CiExecutionMode::Local, &codeyard::backend::ci::CiConfig::default().platform_labels())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.run.event, "pull_request");
    assert!(claim.run.is_fork);
    assert_eq!(claim.run.id, waiting.id);
    assert!(!claim.run.approval_required);
    let groups = db::action_concurrency::groups(f.pool(), repo.id, Some(claim.run.id))
        .await
        .unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].name, "fork-refs/pull/1/merge");
    assert!(claim.secrets.is_empty());
    assert_eq!(claim.run.workflow_path, ".github/workflows/default.yml");
    let fork_token = claim
        .github_tokens
        .workflows
        .values()
        .next()
        .unwrap()
        .values()
        .next()
        .unwrap();
    super::actions_workflows::checked(
        &router,
        "git/create-blob",
        "POST",
        "/api/v3/repos/alice/proj/git/blobs",
        fork_token,
        json!({"content":"forbidden-fork-write"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    let token = super::actions_objects::pat(&f, &router, alice, "write").await;
    super::actions_workflows::checked(
        &router,
        "pulls/update",
        "PATCH",
        "/api/v3/repos/alice/proj/pulls/1",
        &token,
        json!({"title":"Edited title"}),
        StatusCode::OK,
    )
    .await;
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
        .await
        .unwrap();
    assert_eq!(runs.len(), 3, "{runs:?}");
    let edited_run = runs
        .iter()
        .find(|run| run.workflow_path.ends_with("edited.yml"))
        .unwrap();
    assert_eq!(
        edited_run.event_payload.as_ref().unwrap()["action"],
        "edited"
    );
    assert_eq!(edited_run.head_sha.as_deref(), Some(head.as_str()));
    super::actions_workflows::checked(
        &router,
        "pulls/update",
        "PATCH",
        "/api/v3/repos/alice/proj/pulls/1",
        &token,
        json!({"state":"closed"}),
        StatusCode::OK,
    )
    .await;
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
        .await
        .unwrap();
    assert_eq!(runs.len(), 4, "{runs:?}");
    assert_eq!(runs[0].event_payload.as_ref().unwrap()["action"], "closed");
    f.finish().await;
}

async fn dispatch_contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let config = Arc::new(CiConfig {
        trusted_actor_ids: BTreeSet::from([alice]),
        ..Default::default()
    });
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        (*config).clone(),
        None,
    );
    let queue = RunQueue::Database {
        pool: f.pool().clone(),
        config,
        secrets: None,
        context: None,
        notifications: state.notification_broadcaster.clone(),
    };
    let ci = state.ci_runs.clone();
    let router = create_router(state);
    let owner = Client::login(&router, f.pool(), alice).await;
    let source = "name: source\non: workflow_dispatch\npermissions: {contents: write, actions: write}\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{run: echo source}]\n";
    let sha = save(
        &owner,
        "alice",
        ".github/workflows/source.yml",
        "unborn",
        source,
    )
    .await;
    let target = "name: custom\nconcurrency: dispatch-${{ github.event.client_payload.answer }}\non:\n  repository_dispatch:\n    types: [custom]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{run: echo custom}]\n";
    let sha = save(
        &owner,
        "alice",
        ".github/workflows/custom.yml",
        &sha,
        target,
    )
    .await;
    let token = super::actions_objects::pat(&f, &router, alice, "write").await;
    for (kind, expected_count) in [("other", 0), ("custom", 1)] {
        super::actions_workflows::checked(
            &router,
            "repos/create-dispatch-event",
            "POST",
            "/api/v3/repos/alice/proj/dispatches",
            &token,
            json!({"event_type":kind, "client_payload":{"answer":42}}),
            StatusCode::NO_CONTENT,
        )
        .await;
        let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
            .await
            .unwrap();
        assert_eq!(runs.len(), expected_count);
        if let Some(run) = runs.first() {
            assert_eq!(run.event, "repository_dispatch");
            assert_eq!(run.branch, "main");
            assert_eq!(run.head_sha.as_deref(), Some(sha.as_str()));
            let payload = run.event_payload.as_ref().unwrap();
            assert_eq!(payload["action"], kind);
            assert_eq!(payload["client_payload"], json!({"answer":42}));
            assert_eq!(payload["sender"]["id"], alice);
            let groups = db::action_concurrency::groups(f.pool(), repo.id, Some(run.id))
                .await
                .unwrap();
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].name, "dispatch-42");
        }
    }
    super::actions_workflows::checked(
        &router,
        "repos/create-dispatch-event",
        "POST",
        "/api/v3/repos/alice/proj/dispatches",
        &token,
        json!({"event_type":"x".repeat(101)}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    super::actions_workflows::checked(
        &router,
        "actions/create-workflow-dispatch",
        "POST",
        "/api/v3/repos/alice/proj/actions/workflows/source.yml/dispatches",
        &token,
        json!({"ref":"main"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    // Claim the event run first, then the manual source that supplies the job credential.
    let first = queue
        .claim("custom-worker", CiExecutionMode::Local, &codeyard::backend::ci::CiConfig::default().platform_labels())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.run.event, "repository_dispatch");
    let source = queue
        .claim("source-worker", CiExecutionMode::Local, &codeyard::backend::ci::CiConfig::default().platform_labels())
        .await
        .unwrap()
        .unwrap();
    let token = source
        .github_tokens
        .workflows
        .values()
        .next()
        .unwrap()
        .values()
        .next()
        .unwrap();
    super::actions_workflows::checked(
        &router,
        "repos/create-dispatch-event",
        "POST",
        "/api/v3/repos/alice/proj/dispatches",
        token,
        json!({"event_type":"custom", "client_payload":{"from_job":true}}),
        StatusCode::NO_CONTENT,
    )
    .await;
    super::actions_workflows::checked(
        &router,
        "actions/create-workflow-dispatch",
        "POST",
        "/api/v3/repos/alice/proj/actions/workflows/source.yml/dispatches",
        token,
        json!({"ref":"main"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
        .await
        .unwrap();
    assert_eq!(runs.len(), 4);
    assert_eq!(
        runs[1].event_payload.as_ref().unwrap()["sender"]["login"],
        db::ci_tokens::BOT
    );
    assert_eq!(runs[1].triggered_by_id, Some(alice));
    use chrono::Timelike;
    let now = chrono::Utc::now();
    let expression = format!("{} {} * * *", now.minute(), now.hour());
    let schedule = format!("concurrency: scheduled-${{{{ github.event.schedule }}}}\non:\n  schedule:\n    - cron: '{expression}'\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{{run: echo scheduled}}]\n");
    let tip = save(
        &owner,
        "alice",
        ".github/workflows/timed.yml",
        &sha,
        &schedule,
    )
    .await;
    let (first, second) = tokio::join!(ci.queue_schedules(now), ci.queue_schedules(now));
    first.unwrap();
    second.unwrap();
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
        .await
        .unwrap();
    assert_eq!(runs.len(), 5, "{runs:?}");
    let groups = db::action_concurrency::groups(f.pool(), repo.id, Some(runs[0].id))
        .await
        .unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].name, format!("scheduled-{expression}"));
    assert_eq!(runs[0].event, "schedule");
    assert_eq!(runs[0].branch, "main");
    assert_eq!(runs[0].head_sha.as_deref(), Some(tip.as_str()));
    assert_eq!(
        runs[0].event_payload.as_ref().unwrap()["schedule"],
        expression
    );
    f.finish().await;
}

async fn workflow_run_contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let config = Arc::new(CiConfig {
        trusted_actor_ids: BTreeSet::from([alice]),
        ..Default::default()
    });
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        (*config).clone(),
        None,
    );
    let queue = RunQueue::Database {
        pool: f.pool().clone(),
        config,
        secrets: None,
        context: None,
        notifications: state.notification_broadcaster.clone(),
    };
    let router = create_router(state);
    let owner = Client::login(&router, f.pool(), alice).await;
    let source = "name: Source\non: workflow_dispatch\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{run: echo source}]\n";
    let tip = save(
        &owner,
        "alice",
        ".github/workflows/source.yml",
        "unborn",
        source,
    )
    .await;
    let listener = "name: listener\nconcurrency: listener-${{ github.event.action }}\non:\n  workflow_run:\n    workflows: [Source]\n    types: [requested, in_progress, completed]\n    branches: [main]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{run: echo listener}]\n";
    let tip = save(
        &owner,
        "alice",
        ".github/workflows/listener.yml",
        &tip,
        listener,
    )
    .await;
    let tip = save(
        &owner,
        "alice",
        ".github/workflows/wrong-name.yml",
        &tip,
        &listener.replace("[Source]", "[Other]"),
    )
    .await;
    let tip = save(
        &owner,
        "alice",
        ".github/workflows/wrong-branch.yml",
        &tip,
        &listener.replace("[main]", "[feature]"),
    )
    .await;
    let token = super::actions_objects::pat(&f, &router, alice, "write").await;
    super::actions_workflows::checked(
        &router,
        "actions/create-workflow-dispatch",
        "POST",
        "/api/v3/repos/alice/proj/actions/workflows/source.yml/dispatches",
        &token,
        json!({"ref":"main"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let source = queue
        .claim("source-worker", CiExecutionMode::Local, &codeyard::backend::ci::CiConfig::default().platform_labels())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source.run.event, "workflow_dispatch");
    // A repeated claim must not duplicate in_progress listeners.
    queue
        .claim("source-worker", CiExecutionMode::Local, &codeyard::backend::ci::CiConfig::default().platform_labels())
        .await
        .unwrap()
        .unwrap();
    assert!(queue
        .finish("source-worker", source.run.id, "success", None)
        .await
        .unwrap());
    assert!(!queue
        .finish("source-worker", source.run.id, "success", None)
        .await
        .unwrap());
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 30)
        .await
        .unwrap();
    assert_eq!(runs.len(), 4, "{runs:?}");
    let mut activities = BTreeSet::new();
    for run in runs.iter().filter(|run| run.event == "workflow_run") {
        assert_eq!(run.workflow_path, ".github/workflows/listener.yml");
        assert_eq!(run.branch, "main");
        assert_eq!(run.head_sha.as_deref(), Some(tip.as_str()));
        assert!(!run.is_fork);
        let groups = db::action_concurrency::groups(f.pool(), repo.id, Some(run.id))
            .await
            .unwrap();
        assert_eq!(groups.len(), 1);
        let payload = run.event_payload.as_ref().unwrap();
        assert_eq!(
            groups[0].name,
            format!("listener-{}", payload["action"].as_str().unwrap())
        );
        activities.insert(payload["action"].as_str().unwrap().to_owned());
        assert_eq!(payload["workflow_run"]["name"], "Source");
        assert_eq!(payload["workflow_run"]["head_sha"], tip);
        assert_eq!(payload["workflow_run"]["head_branch"], "main");
        assert_eq!(payload["sender"]["id"], alice);
    }
    assert_eq!(
        activities,
        BTreeSet::from(["requested".into(), "in_progress".into(), "completed".into()])
    );
    let mut tip = tip;
    for (name, previous) in [("a", "Source"), ("b", "a"), ("c", "b"), ("d", "c")] {
        let workflow = format!("name: {name}\non:\n  workflow_run:\n    workflows: [{previous}]\n    types: [requested]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{{run: echo chained}}]\n");
        tip = save(
            &owner,
            "alice",
            &format!(".github/workflows/chain-{name}.yml"),
            &tip,
            &workflow,
        )
        .await;
    }
    super::actions_workflows::checked(
        &router,
        "actions/create-workflow-dispatch",
        "POST",
        "/api/v3/repos/alice/proj/actions/workflows/source.yml/dispatches",
        &token,
        json!({"ref":"main"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 30)
        .await
        .unwrap();
    assert_eq!(runs.len(), 9, "{runs:?}");
    let paths: BTreeSet<_> = runs
        .iter()
        .filter(|run| run.workflow_path.contains("chain-"))
        .map(|run| run.workflow_path.as_str())
        .collect();
    assert_eq!(
        paths,
        BTreeSet::from([
            ".github/workflows/chain-a.yml",
            ".github/workflows/chain-b.yml",
            ".github/workflows/chain-c.yml"
        ])
    );
    f.finish().await;
}

/// Runs a contract on its own 32 MiB thread: its deep async state outgrows the default
/// 2 MiB test stack in debug builds.
fn on_a_large_stack<F: std::future::Future<Output = ()>>(contract: impl FnOnce() -> F + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(32 << 20)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(Box::pin(contract()))
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn sqlite_workflow_run_events() {
    on_a_large_stack(|| workflow_run_contract(false));
}

#[test]
fn postgres_workflow_run_events() {
    on_a_large_stack(|| workflow_run_contract(true));
}

#[tokio::test]
async fn sqlite_repository_dispatch_events() {
    dispatch_contract(false).await;
}

#[tokio::test]
async fn postgres_repository_dispatch_events() {
    dispatch_contract(true).await;
}

#[test]
fn sqlite_pull_request_activity_events() {
    on_a_large_stack(|| contract(false));
}

#[test]
fn postgres_pull_request_activity_events() {
    on_a_large_stack(|| contract(true));
}
