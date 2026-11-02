//! Called workflows retain their own OIDC identity and cannot borrow a caller's permission.
use crate::support::database::Database;
use codeyard::backend::{ci, db, repositories};
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
    let actor = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
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
    let run = db::ci::enqueue_ci_run(
        f.pool(),
        db::ci::NewCiRun {
            repo_id: repo.id,
            triggered_by_id: actor,
            pipeline: "full",
            event: "push",
            pull_request_id: None,
            branch: "refs/heads/main",
            head_sha: &sha,
            workflow_path: path,
            workflow_blobs: &json!({}),
            workflow_inputs: None,
            environment: Some("prod:west"),
            manual: false,
        },
        &[actor],
    )
    .await
    .unwrap();
    let run = db::ci::claim_ci_run(f.pool(), "worker", "local", &[actor])
        .await
        .unwrap()
        .filter(|claim| claim.id == run.id)
        .unwrap();
    let credentials = ci::tokens::issue(f.pool(), &run, &definitions, false)
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
    assert!(ci::oidc::issue(f.pool(), &jobs["reader/deploy"], None)
        .await
        .unwrap()
        .is_none());
    let token = ci::oidc::issue(f.pool(), writer, Some("cloud:example"))
        .await
        .unwrap()
        .unwrap();
    let claims = claims(&token);
    assert_eq!(claims["workflow"], "Called");
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
    let token = ci::oidc::issue(f.pool(), nested, None)
        .await
        .unwrap()
        .unwrap();
    let nested_claims = self::claims(&token);
    assert_eq!(nested_claims["workflow"], "Leaf");
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
    assert!(ci::oidc::issue(f.pool(), writer, None)
        .await
        .unwrap()
        .is_none());
    f.execute(
        "UPDATE ci_runs SET cancel_requested=false,is_fork=true WHERE id=$1",
        &[run.id],
    )
    .await;
    assert!(ci::oidc::issue(f.pool(), writer, None)
        .await
        .unwrap()
        .is_none());
    f.execute("UPDATE ci_runs SET is_fork=false WHERE id=$1", &[run.id])
        .await;
    db::ci_tokens::finish_job(f.pool(), run.id, "worker", &format!("{path}:writer/deploy"))
        .await
        .unwrap();
    assert!(ci::oidc::issue(f.pool(), writer, None)
        .await
        .unwrap()
        .is_none());
    assert!(ci::oidc::issue(f.pool(), nested, None)
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
