//! Called workflows are admitted from authorized repositories and run from immutable snapshots.
use crate::support::database::Database;

use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_ci::config::CiExecutionMode;
use codeyard_ci::config::CiPipelineKind;
use codeyard_ci_engine::queue::RunQueue;
use codeyard_ci_engine::workers::CiWorkers;
use codeyard_database::ci_admission::AdmissionError;
use codeyard_git_storage::receive_ci::CiRunRequest;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_storage::ObjectStore;
use codeyard_support::secrets::SecretEncryptor;
use std::{collections::BTreeSet, sync::Arc, time::Duration};

fn commit(path: &str, files: &[(&str, &str)]) -> String {
    let git = git2::Repository::open_bare(path).unwrap();
    let mut workflows = git.treebuilder(None).unwrap();
    for (name, source) in files {
        workflows
            .insert(*name, git.blob(source.as_bytes()).unwrap(), 0o100644)
            .unwrap();
    }
    let mut github = git.treebuilder(None).unwrap();
    github
        .insert("workflows", workflows.write().unwrap(), 0o040000)
        .unwrap();
    let mut root = git.treebuilder(None).unwrap();
    root.insert(".github", github.write().unwrap(), 0o040000)
        .unwrap();
    let tree = git.find_tree(root.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Tester", "test@example.test").unwrap();
    let parent = git.head().ok().and_then(|head| head.peel_to_commit().ok());
    git.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "Workflow",
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
    .to_string()
}

fn request(repo: i64, actor: i64) -> CiRunRequest {
    CiRunRequest {
        repo_id: repo,
        repo_owner: String::new(),
        repo_name: String::new(),
        branch: "main".into(),
        head_sha: None,
        event: "workflow_dispatch".into(),
        pull_request_id: None,
        pipeline: CiPipelineKind::Ci,
        workflow_paths: vec![".github/workflows/ci.yml".into()],
        environment: None,
        inputs: None,
        triggered_by_id: actor,
        manual: true,
    }
}

const CALLED: &str = r#"on:
  workflow_call:
    inputs:
      message: {type: string, required: true}
    secrets:
      password: {required: true}
    outputs:
      message:
        value: ${{ jobs.nested.outputs.message }}
permissions: {contents: read}
jobs:
  nested:
    uses: ./.github/workflows/leaf.yml
    with:
      message: ${{ inputs.message }}
    secrets: inherit
"#;
const LEAF: &str = r#"on:
  workflow_call:
    inputs:
      message: {type: string, required: true}
    secrets:
      password: {required: true}
    outputs:
      message:
        value: ${{ jobs.test.outputs.message }}
jobs:
  test:
    runs-on: ubuntu-latest
    permissions: {contents: read}
    outputs:
      message: ${{ steps.output.outputs.message }}
    steps:
      - id: output
        env:
          PASSWORD: ${{ secrets.password }}
        run: |
          test "$PASSWORD" = reusable-server-password
          test -n '${{ github.token }}'
          test -z "$ACTIONS_ID_TOKEN_REQUEST_TOKEN"
          echo 'message=${{ inputs.message }}' >> "$GITHUB_OUTPUT"
          echo remote-pinned-workflow-ok
"#;

async fn contract(pg: bool) {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("reusable_owner").await;
    let other = f.actor("reusable_other").await;
    let root = tempfile::tempdir().unwrap();
    let config = Arc::new(CiConfig {
        mode: CiExecutionMode::Local,
        actions_binary: std::env::var("CODEYARD_ACTIONS_BIN")
            .expect("real in-house runner required"),
        act_binary: std::env::var("E2E_ACT_BINARY").expect("real act required"),
        trusted_actor_ids: BTreeSet::from([actor]),
        max_active_runs: 1,
        run_timeout_seconds: 60,
        ..CiConfig::default()
    });
    let encryptor = Arc::new(SecretEncryptor::new(&[0x51; 32]));
    let state = AppState::new(
        f.pool().clone(),
        root.path().into(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(encryptor.clone()),
        (*config).clone(),
        None,
    );
    let queue = RunQueue::Database {
        pool: f.pool().clone(),
        config: config.clone(),
        secrets: Some(encryptor.clone()),
        context: None,
        notifications: Arc::new(NotificationBroadcaster::new(f.pool())),
    };
    let remote = codeyard_git_storage::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(other),
        "library",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let caller = codeyard_git_storage::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(actor),
        "caller",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    codeyard_db_actions::secrets::create_or_update_secret(
        f.pool(),
        &encryptor,
        caller.id,
        "CALL_PASSWORD",
        "reusable-server-password",
        Some(actor),
        &codeyard_database::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(f.pool(), actor).await,
        ),
    )
    .await
    .unwrap();
    let caller_yaml = r#"name: Reusable
on: workflow_dispatch
permissions: {contents: read}
jobs:
  call:
    uses: reusable_other/library/.github/workflows/called.yml@main
    strategy:
      max-parallel: 1
      matrix:
        message: [one, two]
    with:
      message: ${{ matrix.message }}
    secrets:
      password: ${{ secrets.CALL_PASSWORD }}
  verify:
    needs: call
    runs-on: ubuntu-latest
    steps:
      - run: |
          test '${{ needs.call.outputs.message }}' = two
          echo caller-output-ok
"#;
    commit(&caller.disk_path, &[("ci.yml", caller_yaml)]);
    let pinned = commit(
        &remote.disk_path,
        &[("called.yml", CALLED), ("leaf.yml", LEAF)],
    );
    let mut admitted = Vec::new();
    for engine in ["codeyard-actions", "act"] {
        f.execute(
            &format!("UPDATE repositories SET actions_engine='{engine}' WHERE id=$1"),
            &[caller.id],
        )
        .await;
        let run = state
            .ci_runs
            .trigger_run(request(caller.id, actor))
            .await
            .unwrap();
        assert_eq!(run.workflow_blobs.as_object().unwrap().len(), 3);
        for value in run
            .workflow_blobs
            .as_object()
            .unwrap()
            .values()
            .filter(|value| value["repository_id"] == remote.id)
        {
            assert_eq!(value["sha"], pinned);
        }
        // Engine selection is made at claim time.
        let claim = queue
            .claim(
                &format!("reusable-{engine}"),
                CiExecutionMode::Local,
                &codeyard_ci::config::CiConfig::default().platform_labels(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.run.engine.as_deref(), Some(engine));
        let tokens = f
            .scalar(
                "SELECT COUNT(*) FROM actions_github_tokens WHERE run_id=$1",
                &[run.id],
            )
            .await;
        assert_eq!(tokens, if engine == "act" { 1 } else { 4 });
        let policies: Vec<String> = match f.pool() {
            codeyard_database::DbPool::Postgres(pool) => {
                sqlx::query_scalar("SELECT permissions FROM actions_github_tokens WHERE run_id=$1")
                    .bind(run.id)
                    .fetch_all(pool)
                    .await
                    .unwrap()
            }
            codeyard_database::DbPool::Sqlite(pool) => {
                sqlx::query_scalar("SELECT permissions FROM actions_github_tokens WHERE run_id=$1")
                    .bind(run.id)
                    .fetch_all(pool)
                    .await
                    .unwrap()
            }
        };
        for policy in policies {
            let policy: codeyard_types::permissions::Permissions =
                serde_json::from_str(&policy).unwrap();
            assert!(policy.allows("contents", false));
            assert!(!policy.allows("contents", true));
            assert!(!policy.allows("issues", false));
            assert!(!policy.allows("id-token", true));
        }
        admitted.push(claim);
        // Release this claim without executing it; the worker-loop runs below use new admissions.
        queue
            .finish(&format!("reusable-{engine}"), run.id, "success", None)
            .await
            .unwrap();
    }
    // A tag wins over a branch of the same name, as on GitHub.
    let library = git2::Repository::open_bare(&remote.disk_path).unwrap();
    let pinned_oid = git2::Oid::from_str(&pinned).unwrap();
    library
        .reference("refs/tags/v1", pinned_oid, false, "tag")
        .unwrap();
    let branch = commit(
        &remote.disk_path,
        &[
            ("called.yml", CALLED),
            ("leaf.yml", LEAF),
            ("other.yml", LEAF),
        ],
    );
    library
        .reference(
            "refs/heads/v1",
            git2::Oid::from_str(&branch).unwrap(),
            false,
            "branch",
        )
        .unwrap();
    library
        .reference("refs/heads/main", pinned_oid, true, "restore")
        .unwrap();
    commit(
        &caller.disk_path,
        &[(
            "ci.yml",
            &caller_yaml.replace("called.yml@main", "called.yml@v1"),
        )],
    );
    let run = state
        .ci_runs
        .trigger_run(request(caller.id, actor))
        .await
        .unwrap();
    for value in run
        .workflow_blobs
        .as_object()
        .unwrap()
        .values()
        .filter(|value| value["repository_id"] == remote.id)
    {
        assert_eq!(value["sha"], pinned);
    }
    queue
        .claim(
            "reusable-tag",
            CiExecutionMode::Local,
            &codeyard_ci::config::CiConfig::default().platform_labels(),
        )
        .await
        .unwrap()
        .unwrap();
    queue
        .finish("reusable-tag", run.id, "success", None)
        .await
        .unwrap();
    commit(&caller.disk_path, &[("ci.yml", caller_yaml)]);
    // Access is checked before private source bytes are admitted, and again before a claim is delivered.
    f.execute(
        "UPDATE repositories SET is_private=true WHERE id=$1",
        &[remote.id],
    )
    .await;
    assert!(matches!(
        state.ci_runs.trigger_run(request(caller.id, actor)).await,
        Err(AdmissionError::Denied("Called workflow access denied"))
    ));
    f.execute(
        "UPDATE repositories SET is_private=false WHERE id=$1",
        &[remote.id],
    )
    .await;
    let revoked = state
        .ci_runs
        .trigger_run(request(caller.id, actor))
        .await
        .unwrap();
    f.execute(
        "UPDATE repositories SET is_private=true WHERE id=$1",
        &[remote.id],
    )
    .await;
    assert!(queue
        .claim(
            "reusable-denied",
            CiExecutionMode::Local,
            &codeyard_ci::config::CiConfig::default().platform_labels()
        )
        .await
        .is_err());
    let failed = codeyard_db_actions::ci::get_ci_run_by_run_id(f.pool(), revoked.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, "failed");
    f.execute(
        "UPDATE repositories SET is_private=false WHERE id=$1",
        &[remote.id],
    )
    .await;
    // A called workflow or job may only lower the caller's token permissions. Asking for more
    // fails the run, as GitHub reports "The nested job is requesting ... but is only allowed".
    let escalated = "permissions: {contents: write, issues: write, id-token: write}";
    for (called, leaf) in [
        (
            CALLED.replace("permissions: {contents: read}", escalated),
            LEAF.to_owned(),
        ),
        (
            CALLED.to_owned(),
            LEAF.replace("permissions: {contents: read}", escalated),
        ),
    ] {
        commit(
            &remote.disk_path,
            &[("called.yml", &called), ("leaf.yml", &leaf)],
        );
        let run = state
            .ci_runs
            .trigger_run(request(caller.id, actor))
            .await
            .unwrap();
        let error = queue
            .claim(
                "reusable-escalated",
                CiExecutionMode::Local,
                &codeyard_ci::config::CiConfig::default().platform_labels(),
            )
            .await
            .err()
            .expect("raised permissions must fail the claim");
        assert!(
            format!("{error:#}").contains("exceed caller permissions"),
            "{error:#}"
        );
        let failed = codeyard_db_actions::ci::get_ci_run_by_run_id(f.pool(), run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, "failed");
        let tokens = f
            .scalar(
                "SELECT COUNT(*) FROM actions_github_tokens WHERE run_id=$1",
                &[run.id],
            )
            .await;
        assert_eq!(tokens, 0);
    }
    commit(
        &remote.disk_path,
        &[("called.yml", CALLED), ("leaf.yml", LEAF)],
    );
    drop(admitted);
    for (engine, nested) in [("codeyard-actions", true), ("act", false), ("act", true)] {
        let source = if !nested {
            caller_yaml.replace("called.yml@main", "leaf.yml@main")
        } else if engine == "act" {
            // act only updates the immediate caller when a nested workflow finishes.
            caller_yaml.replace("  verify:\n    needs: call", "  verify:\n    if: always()\n    needs: call")
                .replace("test '${{ needs.call.outputs.message }}' = two\n          echo caller-output-ok",
                    "test -z '${{ needs.call.result }}'\n          test -z '${{ needs.call.outputs.message }}'\n          echo act-nested-output-limit")
        } else {
            caller_yaml.to_owned()
        };
        commit(&caller.disk_path, &[("ci.yml", &source)]);
        f.execute(
            &format!("UPDATE repositories SET actions_engine='{engine}' WHERE id=$1"),
            &[caller.id],
        )
        .await;
        let run = state
            .ci_runs
            .trigger_run(request(caller.id, actor))
            .await
            .unwrap();
        // Move the remote ref after admission. The worker must execute the old contents.
        commit(
            &remote.disk_path,
            &[(
                "called.yml",
                "on: workflow_call\njobs: {broken: {runs-on: ubuntu-latest, steps: [{run: exit 42}]}}",
            )],
        );
        let workers = CiWorkers::new(queue.clone(), config.clone(), true, root.path());
        workers.startup().await.unwrap();
        let finished = tokio::time::timeout(Duration::from_secs(80), async {
            loop {
                let current = codeyard_db_actions::ci::get_ci_run_by_run_id(f.pool(), run.run_id)
                    .await
                    .unwrap()
                    .unwrap();
                if current.completed_at.is_some() {
                    break current;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("reusable workflow must finish");
        let logs = codeyard_db_actions::ci::list_ci_logs(f.pool(), run.id, 1000)
            .await
            .unwrap();
        assert_eq!(finished.status, "success", "{engine}: {logs:?}");
        assert!(
            logs.iter()
                .any(|line| line.line.contains("remote-pinned-workflow-ok")),
            "{logs:?}"
        );
        let marker = if engine == "act" && nested {
            "act-nested-output-limit"
        } else {
            "caller-output-ok"
        };
        assert!(
            logs.iter().any(|line| line.line.contains(marker)),
            "{logs:?}"
        );
        assert!(
            !logs
                .iter()
                .any(|line| line.line.contains("reusable-server-password")),
            "{logs:?}"
        );
        workers.shutdown().await.unwrap();
        commit(
            &remote.disk_path,
            &[("called.yml", CALLED), ("leaf.yml", LEAF)],
        );
    }
    f.finish().await;
}

#[tokio::test]
#[ignore = "needs a writable delegated cgroup v2 for local CI runs (CI_PROCESS_CGROUP_ROOT)"]
async fn sqlite_reusable_workflows() {
    contract(false).await;
}
#[tokio::test]
#[ignore = "needs a writable delegated cgroup v2 for local CI runs (CI_PROCESS_CGROUP_ROOT)"]
async fn postgres_reusable_workflows() {
    contract(true).await;
}
