//! Called workflows retain their own OIDC identity and cannot borrow a caller's permission.
use crate::support::database::Database;

use codeyard_app::ci;
use codeyard_app::repositories;
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn claims(jwt: &str) -> Value {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(jwt.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap()
}

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let signing_keys = codeyard_ci::oidc::SigningKeys::new(f.pool().clone());
    let actor = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(actor),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let path = ".github/workflows/ci.yml";
    let called = ".github/workflows/called.yml";
    let leaf = ".github/workflows/leaf.yml";
    let definitions = BTreeMap::from([
        (path.into(), b"name: Caller\npermissions: {id-token: write}\njobs:\n  writer: {uses: './.github/workflows/called.yml'}\n  reader: {permissions: {contents: read}, uses: './.github/workflows/called.yml'}\n".to_vec()),
        (called.into(), b"name: Called\njobs:\n  deploy: {environment: 'prod:west'}\n  nested: {uses: './.github/workflows/leaf.yml'}\n".to_vec()),
        (leaf.into(), b"name: Leaf\njobs: {identity: {}}\n".to_vec()),
    ]);
    let sha = "a".repeat(40);
    let run = codeyard_db_actions::ci::enqueue_ci_run(
        f.pool(),
        codeyard_db_actions::ci::NewCiRun {
            repo_id: repo.id,
            triggered_by_id: actor,
            pipeline: "full",
            event: "push",
            pull_request_id: None,
            branch: "refs/heads/main",
            head_sha: &sha,
            workflow_path: path,
            workflow_blobs: &json!({
                (called): {"repository_id": repo.id, "source_path": called, "sha": sha, "full_ref": "refs/heads/main"},
                (leaf): {"repository_id": repo.id, "source_path": leaf, "sha": sha, "full_ref": "refs/heads/main"}
            }),
            workflow_inputs: None,
            environment: Some("prod:west"),
            manual: false,
        },
        &[actor],
    )
    .await
    .unwrap();
    let run = codeyard_db_actions::ci::claim_ci_run(
        f.pool(),
        "worker",
        "local",
        &[actor],
        &codeyard_ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .filter(|claim| claim.id == run.id)
    .unwrap();
    let credentials = codeyard_ci::tokens::issue(f.pool(), &run, &definitions, false)
        .await
        .unwrap();
    let jobs = &credentials.workflows[path];
    let writer = &jobs["writer/deploy"];
    let nested = &jobs["writer/nested/identity"];
    assert!(credentials
        .oidc_jobs
        .contains(&format!("{path}:writer/deploy")));
    assert!(!credentials
        .oidc_jobs
        .contains(&format!("{path}:reader/deploy")));
    assert!(
        codeyard_ci::oidc::issue(&signing_keys, &jobs["reader/deploy"], None)
            .await
            .unwrap()
            .is_none()
    );
    let token = codeyard_ci::oidc::issue(&signing_keys, writer, Some("cloud:example"))
        .await
        .unwrap()
        .unwrap();
    let claims = claims(&token);
    assert_eq!(claims["workflow"], "Caller");
    assert_eq!(
        claims["job_workflow_ref"],
        format!("alice/project/{called}@refs/heads/main")
    );
    // The caller's root workflow comes from the credential, not from the called file.
    assert_eq!(
        claims["workflow_ref"],
        format!("alice/project/{path}@refs/heads/main")
    );
    assert_eq!(claims["sub"], "repo:alice/project:environment:prod%3Awest");
    assert_eq!(claims["environment"], "prod:west");
    assert_eq!(claims["aud"], "cloud:example");
    let token = codeyard_ci::oidc::issue(&signing_keys, nested, None)
        .await
        .unwrap()
        .unwrap();
    let nested_claims = self::claims(&token);
    assert_eq!(nested_claims["workflow"], "Caller");
    assert_eq!(
        nested_claims["job_workflow_ref"],
        format!("alice/project/{leaf}@refs/heads/main")
    );
    assert_eq!(
        nested_claims["sub"],
        "repo:alice/project:ref:refs/heads/main"
    );
    assert!(nested_claims.get("environment").is_none());
    f.execute(
        "UPDATE ci_runs SET cancel_requested=true WHERE id=$1",
        &[run.id],
    )
    .await;
    assert!(codeyard_ci::oidc::issue(&signing_keys, writer, None)
        .await
        .unwrap()
        .is_none());
    f.execute(
        "UPDATE ci_runs SET cancel_requested=false,is_fork=true WHERE id=$1",
        &[run.id],
    )
    .await;
    assert!(codeyard_ci::oidc::issue(&signing_keys, writer, None)
        .await
        .unwrap()
        .is_none());
    f.execute("UPDATE ci_runs SET is_fork=false WHERE id=$1", &[run.id])
        .await;
    codeyard_db_actions::ci_tokens::finish_job(
        f.pool(),
        run.id,
        "worker",
        &format!("{path}:writer/deploy"),
    )
    .await
    .unwrap();
    assert!(codeyard_ci::oidc::issue(&signing_keys, writer, None)
        .await
        .unwrap()
        .is_none());
    assert!(codeyard_ci::oidc::issue(&signing_keys, nested, None)
        .await
        .unwrap()
        .is_some());
    f.finish().await;
}

#[tokio::test]
async fn oidc_called_workflow_identity_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn oidc_called_workflow_identity_postgres() {
    contract(true).await;
}
