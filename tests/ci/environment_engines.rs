//! Both real engines receive job-bound scopes, including Unicode case aliases.
use crate::support::database::Database;
use codeyard::backend::{
    app::AppState,
    ci::{queue::RunQueue, CiConfig, CiExecutionMode, CiPipelineKind, CiRunRequest, CiWorkers},
    crypto::SecretEncryptor,
    db::{
        self,
        environment_values::{self as values, Kind, Save},
    },
    notifications::NotificationBroadcaster,
    repositories::lifecycle,
    storage::ObjectStore,
};
use std::{collections::BTreeSet, sync::Arc, time::Duration};

fn scoped_workflow() -> String {
    let mut yaml = "name: Scoped values\non: workflow_dispatch\njobs:\n".to_owned();
    for (job, environment, secret, region, only) in [
        (
            "ordinary",
            None,
            "repository-private-marker",
            "repo-visible",
            "",
        ),
        (
            "deploy_a",
            Some("İ"),
            "environment-private-marker",
            "env-visible",
            "only-environment-private-marker",
        ),
        (
            "deploy_b",
            Some("i\u{307}"),
            "environment-private-marker",
            "env-visible",
            "only-environment-private-marker",
        ),
    ] {
        yaml.push_str(&format!("  {job}:\n    runs-on: ubuntu-latest\n"));
        if let Some(environment) = environment {
            yaml.push_str(&format!("    environment: '{environment}'\n"));
        }
        yaml.push_str("    steps:\n      - env:\n          TOKEN: ${{ secrets.TOKEN }}\n          ONLY: ${{ secrets.ENV_ONLY }}\n          REGION: ${{ vars.REGION }}\n          PUBLIC_ENV: ${{ vars.PUBLIC_ENV }}\n        run: |\n");
        yaml.push_str(&format!("          test \"$TOKEN\" = '{secret}'\n          test \"$ONLY\" = '{only}'\n          test \"$REGION\" = '{region}'\n"));
        yaml.push_str(if environment.is_some() {
            "          test \"$PUBLIC_ENV\" = 'environment-visible-only'\n"
        } else {
            "          test -z \"$PUBLIC_ENV\"\n"
        });
        yaml.push_str("          test -z \"${ENV_ONLY:-}\"\n          test -z \"${UNEXPORTED:-}\"\n          echo \"$TOKEN\"\n          echo \"$ONLY\"\n          echo \"$REGION\"\n          echo \"$PUBLIC_ENV\"\n");
        yaml.push_str(&format!("          echo 'job-{job}-passed'\n"));
    }
    yaml
}

fn workflow_commit(path: &str, yaml: &str) {
    let git = git2::Repository::open_bare(path).unwrap();
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
        "Scoped environments",
        &tree,
        &[],
    )
    .unwrap();
}

async fn real_engines(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("environment_runner").await;
    let root = tempfile::tempdir().unwrap();
    let config = Arc::new(CiConfig {
        mode: CiExecutionMode::Local,
        actions_binary: std::env::var("CODEYARD_ACTIONS_BIN")
            .expect("CODEYARD_ACTIONS_BIN must name the real native runner"),
        act_binary: std::env::var("E2E_ACT_BINARY").expect("E2E_ACT_BINARY must name patched act"),
        trusted_actor_ids: BTreeSet::from([actor]),
        max_active_runs: 1,
        run_timeout_seconds: 60,
        ..CiConfig::default()
    });
    let encryptor = Arc::new(SecretEncryptor::new(&[0x63; 32]));
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
    for (engine, name) in [("codeyard-actions", "native"), ("act", "act")] {
        let repo = lifecycle::provision(
            (f.pool(), None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(actor),
            name,
            None,
            true,
            None,
        )
        .await
        .unwrap();
        workflow_commit(&repo.disk_path, &scoped_workflow());
        db::secrets::create_or_update_secret(
            f.pool(),
            &encryptor,
            repo.id,
            "TOKEN",
            "repository-private-marker",
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
            Some("repo-visible"),
        )
        .await
        .unwrap();
        values::create(f.pool(), actor, repo.id, "İ").await.unwrap();
        for (kind, name, value) in [
            (Kind::Secret, "TOKEN", "environment-private-marker"),
            (Kind::Secret, "ENV_ONLY", "only-environment-private-marker"),
            (Kind::Variable, "REGION", "env-visible"),
            (Kind::Variable, "PUBLIC_ENV", "environment-visible-only"),
            (Kind::Variable, "UNEXPORTED", "never-auto-exported"),
        ] {
            values::save(
                f.pool(),
                Some(&encryptor),
                actor,
                repo.id,
                "i\u{307}",
                &Save {
                    kind,
                    previous: None,
                    name,
                    value: Some(value),
                },
            )
            .await
            .unwrap();
        }
        f.execute(
            &format!("UPDATE repositories SET actions_engine='{engine}' WHERE id=$1"),
            &[repo.id],
        )
        .await;
        let run = state
            .ci_runs
            .trigger_run(CiRunRequest {
                repo_id: repo.id,
                repo_owner: "environment_runner".into(),
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
        .expect("environment workflow must finish");
        let logs = db::ci::list_ci_logs(f.pool(), run.id, 2000).await.unwrap();
        assert_eq!(finished.status, "success", "{engine}: {logs:?}");
        for job in ["ordinary", "deploy_a", "deploy_b"] {
            assert!(
                logs.iter()
                    .any(|line| line.line.contains(&format!("job-{job}-passed"))),
                "{logs:?}"
            );
        }
        for visible in ["repo-visible", "env-visible", "environment-visible-only"] {
            assert!(
                logs.iter().any(|line| line.line.contains(visible)),
                "{logs:?}"
            );
        }
        for secret in [
            "repository-private-marker",
            "environment-private-marker",
            "only-environment-private-marker",
        ] {
            assert!(
                !logs.iter().any(|line| line.line.contains(secret)),
                "{logs:?}"
            );
        }
    }
    workers.shutdown().await.unwrap();
    f.finish().await;
}

async fn fork_claims(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("environment_fork_runner").await;
    let root = tempfile::tempdir().unwrap();
    let config = Arc::new(CiConfig {
        mode: CiExecutionMode::Local,
        trusted_actor_ids: BTreeSet::from([actor]),
        ..CiConfig::default()
    });
    let encryptor = Arc::new(SecretEncryptor::new(&[0x63; 32]));
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "claims",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    // The claims below include a fork run, which a private repository runs only once allowed.
    db::action_settings::settings(
        f.pool(),
        actor,
        &db::action_settings::Scope::Repository(repo.id),
        db::action_settings::Setting::ForkWorkflows,
        Some(&serde_json::json!({"run_workflows_from_fork_pull_requests": true})),
    )
    .await
    .unwrap();
    workflow_commit(&repo.disk_path,"on: workflow_dispatch\njobs:\n  deploy:\n    runs-on: ubuntu-latest\n    environment: İ\n    steps: [{run: 'true'}]\n");
    values::create(f.pool(), actor, repo.id, "İ").await.unwrap();
    for (kind, name, value) in [
        (Kind::Secret, "TOKEN", "environment-private-claim-marker"),
        (Kind::Variable, "REGION", "env-visible"),
    ] {
        values::save(
            f.pool(),
            Some(&encryptor),
            actor,
            repo.id,
            "İ",
            &Save {
                kind,
                previous: None,
                name,
                value: Some(value),
            },
        )
        .await
        .unwrap();
    }
    db::secrets::create_or_update_secret(
        f.pool(),
        &encryptor,
        repo.id,
        "TOKEN",
        "repository-private-claim-marker",
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
        Some("repo-visible"),
    )
    .await
    .unwrap();
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(encryptor),
        (*config).clone(),
        None,
    );
    // No master key on either claim: forks need none; an environment secret alone must refuse a non-fork claim.
    let queue = RunQueue::Database {
        pool: f.pool().clone(),
        config,
        secrets: None,
        context: None,
        notifications: Arc::new(NotificationBroadcaster::new(f.pool())),
    };
    for fork in [true, false] {
        let run = state
            .ci_runs
            .trigger_run(CiRunRequest {
                repo_id: repo.id,
                repo_owner: "environment_fork_runner".into(),
                repo_name: "claims".into(),
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
        if fork {
            f.execute("UPDATE ci_runs SET is_fork=true WHERE id=$1", &[run.id])
                .await;
            let claim = queue
                .claim("worker", CiExecutionMode::Local, &codeyard::backend::ci::CiConfig::default().platform_labels())
                .await
                .unwrap()
                .unwrap();
            assert!(claim.secrets.is_empty());
            assert_eq!(claim.variables, [("REGION".into(), "repo-visible".into())]);
            assert!(claim.environment_values.environments["i\u{307}"]
                .secrets
                .is_empty());
            assert_eq!(
                claim.environment_values.environments["i\u{307}"].variables["REGION"],
                "env-visible"
            );
            let encoded = serde_json::to_string(&claim).unwrap();
            assert!(!encoded.contains("private-claim-marker"));
            queue
                .finish("worker", run.id, "success", None)
                .await
                .unwrap();
            db::secrets::delete_secret(f.pool(), actor, repo.id, "TOKEN", &db::users::accounts::AccountCredential::Session(&crate::support::live_session(f.pool(), actor).await))
                .await
                .unwrap();
        } else {
            let error = queue
                .claim("worker", CiExecutionMode::Local, &codeyard::backend::ci::CiConfig::default().platform_labels())
                .await
                .err()
                .expect("environment secret must require the master key");
            assert!(!error.to_string().contains("private-claim-marker"));
            assert_eq!(
                db::ci::get_ci_run_by_run_id(f.pool(), run.run_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                "failed"
            );
        }
    }
    f.finish().await;
}

#[tokio::test]
async fn sqlite_environment_values_fork_claims() {
    fork_claims(false).await;
}
#[tokio::test]
async fn postgres_environment_values_fork_claims() {
    fork_claims(true).await;
}

#[tokio::test]
async fn sqlite_environment_values_real_engines() {
    real_engines(false).await;
}
#[tokio::test]
async fn postgres_environment_values_real_engines() {
    real_engines(true).await;
}
