//! Real native attempts preserve pinned content, identities, counters and runner contexts.
use super::{actions_objects::pat, actions_workflows::checked};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use axum::http::StatusCode;

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_ci::config::CiExecutionMode;
use codeyard_ci::queue::RunQueue;
use codeyard_ci::workers::CiWorkers;
use codeyard_git_storage::receive_ci::CiRunRequest;
use codeyard_git_storage::repositories::store::repo_store;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_repos::editor::FileWrite;
use codeyard_storage::ObjectStore;
use codeyard_types::ci::CiPipelineKind;
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc, time::Duration};

fn workflow_commit(path: &str, yaml: &str) -> String {
    let git = git2::Repository::open_bare(path).unwrap();
    let mut workflows = git.treebuilder(None).unwrap();
    workflows
        .insert("ci.yml", git.blob(yaml.as_bytes()).unwrap(), 0o100644)
        .unwrap();
    let mut github = git.treebuilder(None).unwrap();
    github
        .insert("workflows", workflows.write().unwrap(), 0o040000)
        .unwrap();
    let mut tree = git.treebuilder(None).unwrap();
    tree.insert(".github", github.write().unwrap(), 0o040000)
        .unwrap();
    let tree = git.find_tree(tree.write().unwrap()).unwrap();
    let author = git2::Signature::now("Alice", "alice@example.test").unwrap();
    git.commit(
        Some("refs/heads/main"),
        &author,
        &author,
        "Workflow",
        &tree,
        &[],
    )
    .unwrap()
    .to_string()
}

async fn finished(
    pool: &codeyard_database::DbPool,
    run: uuid::Uuid,
) -> codeyard_database::models::CiRun {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let run = codeyard_db_actions::ci::get_ci_run_by_run_id(pool, run)
                .await
                .unwrap()
                .unwrap();
            if run.completed_at.is_some() {
                return run;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("native attempt must finish")
}

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let config = Arc::new(CiConfig {
        mode: CiExecutionMode::Local,
        actions_binary: std::env::var("CODEYARD_ACTIONS_BIN").expect("real native binary required"),
        trusted_actor_ids: BTreeSet::from([alice, bob]),
        max_active_runs: 1,
        run_timeout_seconds: 30,
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
    let api = create_router(state.clone());
    let repo = codeyard_git_storage::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        codeyard_database::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let yaml = r#"name: Pinned
on: workflow_dispatch
permissions: {contents: read}
jobs:
  context:
    runs-on: ubuntu-latest
    steps:
      - run: |
          test '${{ github.actor }}' = alice
          test "$GITHUB_ACTOR" = alice
          test '${{ github.actor_id }}' = ALICE_ID
          test '${{ github.run_number }}' = 1
          test '${{ github.run_attempt }}' = "$GITHUB_RUN_ATTEMPT"
          if [ "$GITHUB_RUN_ATTEMPT" = 1 ]; then
            test '${{ github.triggering_actor }}' = alice
          else
            test '${{ github.triggering_actor }}' = bob
            test "$ACTIONS_STEP_DEBUG" = true
            test '${{ runner.debug }}' = 1
          fi
          echo actual-attempt=$GITHUB_RUN_ATTEMPT
          echo '::add-mask::private-diagnostic'
          echo '::warning file=src/main.rs,line=7,endLine=9,title=Compiler%2C warning::masked private-diagnostic%0Asecond line'
          echo '::stop-commands::pause'
          echo '::error file=not-an-annotation,line=99::ordinary output'
          echo '::pause::'
"#
    .replace("ALICE_ID", &alice.to_string());
    let sha = workflow_commit(&repo.disk_path, &yaml);
    let reader = pat(&f, &api, alice, "read").await;
    let writer = pat(&f, &api, bob, "write").await;
    let base = "/api/v3/repos/alice/project/actions";
    checked(
        &api,
        "actions/list-repo-workflows",
        "GET",
        &format!("{base}/workflows"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let original = state
        .ci_runs
        .trigger_run(CiRunRequest {
            repo_id: repo.id,
            repo_owner: "alice".into(),
            repo_name: "project".into(),
            branch: "main".into(),
            head_sha: None,
            event: "workflow_dispatch".into(),
            pull_request_id: None,
            pipeline: CiPipelineKind::Ci,
            workflow_paths: vec![".github/workflows/ci.yml".into()],
            environment: None,
            inputs: None,
            triggered_by_id: alice,
            manual: true,
        })
        .await
        .unwrap();
    let rerun = format!("{base}/runs/{}/rerun", original.id);
    checked(
        &api,
        "actions/re-run-workflow",
        "POST",
        &rerun,
        &writer,
        json!({}),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1", &[repo.id])
            .await,
        1
    );
    let workers = CiWorkers::new(
        RunQueue::Database {
            pool: f.pool().clone(),
            config: config.clone(),
            secrets: None,
            context: None,
            notifications: Arc::new(NotificationBroadcaster::new(f.pool())),
        },
        config,
        true,
        root.path(),
    );
    workers.startup().await.unwrap();
    let first = finished(f.pool(), original.run_id).await;
    assert_eq!(
        first.status,
        "success",
        "{:?}",
        codeyard_db_actions::ci::list_ci_logs(f.pool(), first.id, 1000)
            .await
            .unwrap()
    );
    let (_, jobs) = checked(
        &api,
        "actions/list-jobs-for-workflow-run",
        "GET",
        &format!("{base}/runs/{}/jobs?per_page=100", first.id),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(jobs["total_count"], 1);
    let job = jobs["jobs"][0]["id"].as_i64().unwrap();
    super::actions_jobs::assert_gh_job(
        &jobs["jobs"][0],
        first.id,
        job,
        "context",
        "completed",
        json!("success"),
    );
    let (_, annotations) = checked(
        &api,
        "checks/list-annotations",
        "GET",
        &format!("/api/v3/repos/alice/project/check-runs/{job}/annotations"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(annotations.as_array().unwrap().len(), 1);
    assert_eq!(annotations[0]["message"], "masked ***\nsecond line");
    assert_eq!(annotations[0]["path"], "src/main.rs");
    assert_eq!(annotations[0]["start_line"], 7);
    assert_eq!(annotations[0]["end_line"], 9);
    assert_eq!(annotations[0]["annotation_level"], "warning");
    assert_eq!(annotations[0]["title"], "Compiler, warning");
    let store = repo_store(&repo, state.cluster.as_ref());
    store
        .save_file_as(
            f.pool(),
            repo.id,
            alice,
            &FileWrite {
                branch: "main".into(),
                path: ".github/workflows/ci.yml".into(),
                message: "Remove workflow".into(),
                expected_revision: sha.clone(),
                creating: false,
                deleting: true,
            },
            b"",
            "Alice",
            "alice@example.test",
        )
        .await
        .unwrap();
    checked(
        &api,
        "actions/re-run-workflow",
        "POST",
        &rerun,
        &reader,
        json!({}),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &api,
        "actions/re-run-workflow",
        "POST",
        &rerun,
        &writer,
        json!({"enable_debug_logging":"true"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &api,
        "actions/re-run-workflow",
        "POST",
        &rerun,
        &writer,
        json!({"enable_debug_logging":true}),
        StatusCode::CREATED,
    )
    .await;
    let next = codeyard_db_actions::action_runs::get(f.pool(), repo.id, first.id, None)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(next.id, first.id);
    assert_eq!(next.head_sha.as_deref(), Some(sha.as_str()));
    assert_eq!(next.workflow_run_id, Some(first.id));
    assert_eq!(next.workflow_run_number, Some(1));
    assert_eq!(next.workflow_run_attempt, Some(2));
    let second = finished(f.pool(), next.run_id).await;
    let logs = codeyard_db_actions::ci::list_ci_logs(f.pool(), second.id, 1000)
        .await
        .unwrap();
    assert_eq!(second.status, "success", "{logs:?}");
    assert!(
        logs.iter()
            .any(|line| line.line.contains("actual-attempt=2")),
        "{logs:?}"
    );
    let (_, document) = checked(
        &api,
        "actions/get-workflow-run",
        "GET",
        &format!("{base}/runs/{}", first.id),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(document["actor"]["login"], "alice");
    assert_eq!(document["triggering_actor"]["login"], "bob");
    assert_eq!(document["run_attempt"], 2);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_jobs WHERE run_id=$1 AND started_at IS NOT NULL",
            &[first.id]
        )
        .await,
        1
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_jobs WHERE run_id=$1 AND started_at IS NOT NULL",
            &[second.id]
        )
        .await,
        1
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_jobs WHERE run_id=$1 AND matrix IS NOT NULL",
            &[second.id]
        )
        .await,
        1
    );
    assert_eq!(
        f.scalar(
            "SELECT next_run_number FROM actions_workflows WHERE repo_id=$1",
            &[repo.id]
        )
        .await,
        1
    );
    partial(
        &f,
        &state,
        &api,
        (alice, bob),
        (&reader, &writer),
        root.path(),
    )
    .await;
    workers.shutdown().await.unwrap();
}

async fn partial(
    f: &Database,
    state: &AppState,
    api: &axum::Router,
    (alice, bob): (i64, i64),
    (reader, writer): (&str, &str),
    root: &std::path::Path,
) {
    let repo = codeyard_git_storage::repositories::lifecycle::provision(
        (f.pool(), None),
        root,
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
        "partial",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        codeyard_database::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    workflow_commit(
        &repo.disk_path,
        r#"name: Partial
on: workflow_dispatch
jobs:
  producer:
    runs-on: ubuntu-latest
    outputs:
      safe: ${{ steps.values.outputs.safe }}
    steps:
      - id: values
        run: echo safe=observed >> "$GITHUB_OUTPUT"
  build:
    needs: producer
    runs-on: ubuntu-latest
    strategy:
      max-parallel: 1
      matrix:
        index: [0, 1]
    outputs:
      value: ${{ steps.values.outputs.value }}
    steps:
      - id: values
        run: |
          test '${{ needs.producer.outputs.safe }}' = observed
          if [ "$GITHUB_RUN_ATTEMPT" != 1 ]; then
            test "$ACTIONS_STEP_DEBUG" = true
            test '${{ runner.debug }}' = 1
          fi
          if [ '${{ matrix.index }}' = 0 ]; then
            echo value=zero >> "$GITHUB_OUTPUT"
          else
            if [ "$GITHUB_RUN_ATTEMPT" = 1 ]; then exit 1; fi
            test '${{ strategy.job-index }}' = 1
            echo value=one >> "$GITHUB_OUTPUT"
          fi
  dependent:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: test '${{ needs.build.outputs.value }}' = one
"#,
    );
    let original = state
        .ci_runs
        .trigger_run(CiRunRequest {
            repo_id: repo.id,
            repo_owner: "alice".into(),
            repo_name: "partial".into(),
            branch: "main".into(),
            head_sha: None,
            event: "workflow_dispatch".into(),
            pull_request_id: None,
            pipeline: CiPipelineKind::Ci,
            workflow_paths: vec![".github/workflows/ci.yml".into()],
            environment: None,
            inputs: None,
            triggered_by_id: alice,
            manual: true,
        })
        .await
        .unwrap();
    let first = finished(f.pool(), original.run_id).await;
    assert_eq!(
        first.status,
        "failed",
        "{:?}",
        codeyard_db_actions::ci::list_ci_logs(f.pool(), first.id, 1000)
            .await
            .unwrap()
    );
    let base = "/api/v3/repos/alice/partial/actions";
    checked(
        api,
        "actions/re-run-workflow-failed-jobs",
        "POST",
        &format!("{base}/runs/{}/rerun-failed-jobs", first.id),
        writer,
        json!({"enable_debug_logging":true}),
        StatusCode::CREATED,
    )
    .await;
    let next = codeyard_db_actions::action_runs::get(f.pool(), repo.id, first.id, None)
        .await
        .unwrap()
        .unwrap();
    let second = finished(f.pool(), next.run_id).await;
    assert_eq!(
        second.status,
        "success",
        "{:?}",
        codeyard_db_actions::ci::list_ci_logs(f.pool(), second.id, 1000)
            .await
            .unwrap()
    );
    assert_eq!(second.workflow_run_number, Some(1));
    assert_eq!(second.workflow_run_attempt, Some(2));
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_github_tokens WHERE run_id=$1",
            &[second.id]
        )
        .await,
        2
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_github_tokens WHERE run_id=$1 AND job LIKE '%:producer'",
            &[second.id]
        )
        .await,
        0
    );
    let (_, jobs) = checked(
        api,
        "actions/list-jobs-for-workflow-run",
        "GET",
        &format!("{base}/runs/{}/jobs", first.id),
        reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(jobs["total_count"], 2);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_jobs WHERE run_id=$1 AND logical_job='producer'",
            &[second.id]
        )
        .await,
        0
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM actions_jobs WHERE run_id=$1 AND logical_job='build' AND variant=0",&[second.id]).await,0);
    let job = f
        .scalar(
            "SELECT id FROM actions_jobs WHERE run_id=$1 AND logical_job='build' AND variant=1",
            &[second.id],
        )
        .await;
    checked(
        api,
        "actions/re-run-job-for-workflow-run",
        "POST",
        &format!("{base}/jobs/{job}/rerun"),
        writer,
        json!({"enable_debug_logging":true}),
        StatusCode::CREATED,
    )
    .await;
    let next = codeyard_db_actions::action_runs::get(f.pool(), repo.id, first.id, None)
        .await
        .unwrap()
        .unwrap();
    let third = finished(f.pool(), next.run_id).await;
    assert_eq!(
        third.status,
        "success",
        "{:?}",
        codeyard_db_actions::ci::list_ci_logs(f.pool(), third.id, 1000)
            .await
            .unwrap()
    );
    assert_eq!(third.workflow_run_attempt, Some(3));
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_jobs WHERE run_id=$1 AND started_at IS NOT NULL",
            &[third.id]
        )
        .await,
        2
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_jobs WHERE run_id=$1 AND logical_job='producer'",
            &[third.id]
        )
        .await,
        0
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM actions_jobs WHERE run_id=$1 AND logical_job='build' AND variant=0",&[third.id]).await,0);
    checked(
        api,
        "actions/re-run-job-for-workflow-run",
        "POST",
        &format!("{base}/jobs/{job}/rerun"),
        writer,
        json!({}),
        StatusCode::CONFLICT,
    )
    .await;
    checked(
        api,
        "actions/re-run-workflow-failed-jobs",
        "POST",
        &format!("{base}/runs/{}/rerun-failed-jobs", first.id),
        writer,
        json!({}),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1", &[repo.id])
            .await,
        3
    );
    let credential = f
        .scalar(
            "SELECT id FROM personal_access_tokens WHERE user_id=$1",
            &[bob],
        )
        .await;
    let hash = codeyard_support::token_hash::token_hash(writer);
    f.execute(
        "DELETE FROM personal_access_tokens WHERE id=$1",
        &[credential],
    )
    .await;
    let denied = state
        .ci_runs
        .rerun_workflow_run(
            repo.id,
            first.id,
            bob,
            false,
            Some(codeyard_database::queries::ci::CiCredential::Token {
                id: credential,
                hash,
                permission: "actions",
            }),
        )
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("credential"), "{denied}");
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1", &[repo.id])
            .await,
        3
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a writable delegated cgroup v2 for local CI runs (CI_PROCESS_CGROUP_ROOT)"]
async fn sqlite_real_actions_reruns() {
    contract(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a writable delegated cgroup v2 for local CI runs (CI_PROCESS_CGROUP_ROOT)"]
async fn postgres_real_actions_reruns() {
    contract(true).await;
}
