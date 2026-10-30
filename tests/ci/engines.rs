//! Both real runners execute admitted workflows through the CI worker loop.
use crate::support::database::Database;
use codeyard::backend::{
    app::AppState,
    ci::{queue::RunQueue, CiConfig, CiExecutionMode, CiPipelineKind, CiRunRequest, CiWorkers},
    crypto::SecretEncryptor,
    db,
    notifications::NotificationBroadcaster,
    repositories,
    storage::ObjectStore,
};
use std::{collections::BTreeSet, sync::Arc, time::Duration};

fn workflow_commit(path: &str) {
    let git = git2::Repository::open_bare(path).unwrap();
    let step = r#"      - env:
          TOKEN: ${{ github.token }}
          SECRET: ${{ secrets.GITHUB_TOKEN }}
          REPO_SECRET: ${{ secrets.DEPLOY_TOKEN }}
          VARIABLE: ${{ vars.REGION }}
          ORG_SECRET: ${{ secrets.ORG_TOKEN }}
          ORG_VARIABLE: ${{ vars.ORG_REGION }}
          API: ${{ github.api_url }}
          SERVER: ${{ github.server_url }}
          GRAPHQL: ${{ github.graphql_url }}
        run: |
          test -n "$TOKEN"
          test "$TOKEN" = "$SECRET"
          test "$REPO_SECRET" = repository-secret-value
          test "$VARIABLE" = repo-variable
          test "$ORG_SECRET" = organization-secret-value
          test "$ORG_VARIABLE" = organization-variable
          echo "$REPO_SECRET"
          echo "$VARIABLE"
          echo "$ORG_SECRET"
          echo "$ORG_VARIABLE"
          test "$API" = "$SERVER/api/v3"
          test "$GRAPHQL" = "$SERVER/api/graphql"
          printf 'credential-hash='
          printf '%s' "$TOKEN" | sha256sum
"#;
    let yaml = format!("name: Tokens\non: workflow_dispatch\npermissions: {{contents: write}}\njobs:\n  writer:\n    runs-on: linux\n    steps:\n{step}  reader:\n    permissions: {{contents: read}}\n    runs-on: linux\n    steps:\n{step}");
    let mut workflows = git.treebuilder(None).unwrap();
    workflows
        .insert("ci.yml", git.blob(yaml.as_bytes()).unwrap(), 0o100644)
        .unwrap();
    let mut github = git.treebuilder(None).unwrap();
    github
        .insert("workflows", workflows.write().unwrap(), 0o040000)
        .unwrap();
    let mut root = git.treebuilder(None).unwrap();
    root.insert(".github", github.write().unwrap(), 0o040000)
        .unwrap();
    let tree = git.find_tree(root.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Tester", "test@example.test").unwrap();
    git.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "Workflow",
        &tree,
        &[],
    )
    .unwrap();
}

async fn contract(pg: bool) {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("runner_owner").await;
    let root = tempfile::tempdir().unwrap();
    let config = Arc::new(CiConfig {
        mode: CiExecutionMode::Local,
        actions_binary: std::env::var("CODEYARD_ACTIONS_BIN")
            .expect("CODEYARD_ACTIONS_BIN must name the real in-house runner"),
        act_binary: std::env::var("E2E_ACT_BINARY")
            .expect("E2E_ACT_BINARY must name the real act binary"),
        trusted_actor_ids: BTreeSet::from([actor]),
        max_active_runs: 1,
        run_timeout_seconds: 60,
        ..CiConfig::default()
    });
    let encryptor = Arc::new(SecretEncryptor::new(&[0x63; 32]));
    let org = crate::support::organizations::applied(
        db::organizations::create_organization(
            f.pool(),
            actor,
            &db::organizations::NewOrganization::new(
                "runner_org",
                db::organizations::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    for (kind, name, value) in [
        (
            db::organization_values::Kind::Secret,
            "ORG_TOKEN",
            "organization-secret-value",
        ),
        (
            db::organization_values::Kind::Secret,
            "DEPLOY_TOKEN",
            "shadowed-organization-secret",
        ),
        (
            db::organization_values::Kind::Variable,
            "ORG_REGION",
            "organization-variable",
        ),
        (
            db::organization_values::Kind::Variable,
            "REGION",
            "shadowed-organization-variable",
        ),
    ] {
        db::organization_values::save(
            f.pool(),
            Some(&encryptor),
            actor,
            &db::users::accounts::AccountCredential::Trusted,
            org.id,
            &db::organization_values::Save {
                kind,
                previous: None,
                name,
                value: Some(value),
                visibility: Some("all"),
                repositories: None,
            },
        )
        .await
        .unwrap();
    }
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(encryptor.clone()),
        (*config).clone(),
        None,
    );
    let workers = CiWorkers::new(
        RunQueue::Database {
            pool: f.pool().clone(),
            config,
            secrets: Some(encryptor.clone()),
            context: None,
            notifications: Arc::new(NotificationBroadcaster::new(f.pool())),
        },
        Arc::new(state.ci_runs.config().clone()),
        true,
        root.path(),
    );
    workers.startup().await.unwrap();
    for engine in ["codeyard-actions", "act"] {
        let name = if engine == "act" {
            "act-run"
        } else {
            "native-run"
        };
        let repo = repositories::lifecycle::provision(
            (f.pool(), None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner {
                actor_id: actor,
                target: db::repositories::creation::RepositoryTarget::Organization(org.id),
            },
            name,
            None,
            true,
            None,
        )
        .await
        .unwrap();
        workflow_commit(&repo.disk_path);
        db::secrets::create_or_update_secret(
            f.pool(),
            &encryptor,
            repo.id,
            "DEPLOY_TOKEN",
            "repository-secret-value",
            Some(actor),
            &db::users::accounts::AccountCredential::Session(&crate::support::live_session(f.pool(), actor).await),
        )
        .await
        .unwrap();
        db::variables::save(
            f.pool(),
            actor,
            &db::users::accounts::AccountCredential::Session(&crate::support::live_session(f.pool(), actor).await),
            repo.id,
            None,
            "REGION",
            Some("repo-variable"),
        )
        .await
        .unwrap();
        f.execute(
            &format!("UPDATE repositories SET actions_engine='{engine}' WHERE id=$1"),
            &[repo.id],
        )
        .await;
        let run = state
            .ci_runs
            .trigger_run(CiRunRequest {
                repo_id: repo.id,
                repo_owner: "runner_org".into(),
                repo_name: name.into(),
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
            })
            .await
            .unwrap();
        let finished = tokio::time::timeout(Duration::from_secs(80), async {
            loop {
                let current = db::ci::get_ci_run_by_run_id(f.pool(), run.run_id)
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
        .expect("workflow must finish");
        let logs = db::ci::list_ci_logs(f.pool(), run.id, 1000).await.unwrap();
        assert_eq!(finished.status, "success", "{engine}: {logs:?}");
        assert_eq!(finished.engine.as_deref(), Some(engine));
        assert!(
            logs.iter().any(|line| line.line.contains("repo-variable")),
            "{logs:?}"
        );
        assert!(
            !logs
                .iter()
                .any(|line| line.line.contains("repository-secret-value")),
            "{logs:?}"
        );
        assert!(
            !logs
                .iter()
                .any(|line| line.line.contains("organization-secret-value")),
            "{logs:?}"
        );
        assert!(
            logs.iter()
                .any(|line| line.line.contains("organization-variable")),
            "{logs:?}"
        );
        let hashes: Vec<_> = logs
            .iter()
            .filter_map(|line| {
                let (_, tail) = line.line.split_once("credential-hash=")?;
                let hash = tail.get(..64)?;
                hash.bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                    .then(|| hash.to_owned())
            })
            .collect();
        assert_eq!(hashes.len(), 2, "{logs:?}");
        assert_eq!(hashes[0] == hashes[1], engine == "act", "{logs:?}");
        let warnings = logs.iter().filter(|line| line.line == "act runs give every job the same token; use the codeyard-actions engine for per-job permissions").count();
        assert_eq!(warnings, usize::from(engine == "act"));
        assert_eq!(
            f.scalar(
                "SELECT COUNT(*) FROM actions_github_tokens WHERE run_id=$1",
                &[run.id]
            )
            .await,
            if engine == "act" { 1 } else { 2 }
        );
        if engine == "codeyard-actions" {
            assert_eq!(f.scalar("SELECT COUNT(*) FROM actions_github_tokens WHERE run_id=$1 AND revoked_at IS NOT NULL", &[run.id]).await, 2);
            let api = super::actions_workflows::router(&f, root.path(), actor).await;
            let reader = super::actions_objects::pat(&f, &api, actor, "read").await;
            let base = format!("/api/v3/repos/runner_org/{name}/actions");
            let (_, jobs) = super::actions_workflows::checked(
                &api,
                "actions/list-jobs-for-workflow-run",
                "GET",
                &format!("{base}/runs/{}/jobs", run.id),
                &reader,
                serde_json::Value::Null,
                axum::http::StatusCode::OK,
            )
            .await;
            assert_eq!(jobs["total_count"], 2, "{jobs}");
            for job in jobs["jobs"].as_array().unwrap() {
                assert_eq!(job["status"], "completed");
                assert_eq!(job["conclusion"], "success");
                assert_eq!(job["workflow_name"], "Tokens");
                assert_eq!(job["labels"], serde_json::json!(["linux"]));
                assert_eq!(job["head_sha"], finished.head_sha.as_deref().unwrap());
                assert!(
                    job["steps"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|step| step["number"] == 2 && step["conclusion"] == "success"),
                    "{job}"
                );
                let (_, one) = super::actions_workflows::checked(
                    &api,
                    "actions/get-job-for-workflow-run",
                    "GET",
                    &format!("{base}/jobs/{}", job["id"]),
                    &reader,
                    serde_json::Value::Null,
                    axum::http::StatusCode::OK,
                )
                .await;
                assert_eq!(&one, job);
            }
            let (_, attempt) = super::actions_workflows::checked(
                &api,
                "actions/list-jobs-for-workflow-run-attempt",
                "GET",
                &format!("{base}/runs/{}/attempts/1/jobs", run.id),
                &reader,
                serde_json::Value::Null,
                axum::http::StatusCode::OK,
            )
            .await;
            assert_eq!(attempt, jobs);
            assert_eq!(
                f.scalar(
                    "SELECT COUNT(DISTINCT job_id) FROM ci_logs WHERE run_id=$1 AND step_number=2",
                    &[run.id]
                )
                .await,
                2
            );
            assert_eq!(f.scalar("SELECT COUNT(*) FROM actions_job_states WHERE run_id=$1 AND CAST(state AS TEXT) LIKE '%ghs_%'",&[run.id]).await,0);
        }
    }
    workers.shutdown().await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn sqlite_real_workflow_on_each_engine() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_real_workflow_on_each_engine() {
    contract(true).await;
}
