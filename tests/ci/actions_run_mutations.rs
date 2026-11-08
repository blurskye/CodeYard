//! Run mutations check scoped credentials again at the effect and retain runtime cleanup.
use super::{
    actions_objects::{artifact, checked, pat},
    results,
};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use axum::http::StatusCode;

use codeyard_db_actions::ci_admission::AdmissionError;

use codeyard_db_actions::ci::CiRunDeletion;
use codeyard_types::ci::LogLine;
use std::collections::BTreeMap;

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for name in ["project", "other"] {
        repos.push(
            codeyard_git_storage::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
                name,
                None,
                true,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let repo = &repos[0];
    let router = f.router(root.path()).await;
    let writer = pat(&f, &router, alice, "write").await;
    let reader = pat(&f, &router, alice, "read").await;
    let (source, _) = results::run(&f, repo.id, alice, "main").await;
    let definitions=BTreeMap::from([(".github/workflows/ci.yml".to_owned(),b"on: push\njobs:\n  reader:\n    permissions: {actions: read}\n    runs-on: ubuntu-latest\n    steps: [{run: 'echo ok'}]\n  writer:\n    permissions: {actions: write}\n    runs-on: ubuntu-latest\n    steps: [{run: 'echo ok'}]\n".to_vec())]);
    let credentials = codeyard_ci::tokens::issue(f.pool(), &source, &definitions, false)
        .await
        .unwrap();
    let read_token = &credentials.workflows[".github/workflows/ci.yml"]["reader"];
    let write_token = &credentials.workflows[".github/workflows/ci.yml"]["writer"];
    let read_pat = codeyard_db_identity::users::tokens::find_token(
        f.pool(),
        &codeyard_security::token_hash::token_hash(read_token),
    )
    .await
    .unwrap()
    .unwrap();
    let write_pat = codeyard_db_identity::users::tokens::find_token(
        f.pool(),
        &codeyard_security::token_hash::token_hash(write_token),
    )
    .await
    .unwrap()
    .unwrap();
    let (queued, _) = results::run(&f, repo.id, alice, "main").await;
    f.execute(
        "UPDATE ci_runs SET status='queued',worker=NULL WHERE id=$1",
        &[queued.id],
    )
    .await;
    let base = "/api/v3/repos/alice/project/actions/runs";
    let cancel = format!("{base}/{}/cancel", queued.id);
    checked(
        &router,
        "actions/cancel-workflow-run",
        "POST",
        &cancel,
        Some(&reader),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "actions/cancel-workflow-run",
        "POST",
        &cancel,
        Some(read_token),
        StatusCode::FORBIDDEN,
    )
    .await;
    let refusal = codeyard_db_actions::ci::request_ci_cancellation_with_credential(
        f.pool(),
        queued.run_id,
        alice,
        &codeyard_database::queries::ci::CiCredential::Token {
            id: read_pat.id,
            hash: codeyard_security::token_hash::token_hash(read_token),
            permission: "actions",
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        AdmissionError::from(refusal),
        AdmissionError::Denied(_)
    ));
    checked(
        &router,
        "actions/cancel-workflow-run",
        "POST",
        &cancel,
        Some(write_token),
        StatusCode::ACCEPTED,
    )
    .await;
    assert_eq!(
        codeyard_db_actions::ci::get_ci_run_by_id(f.pool(), repo.id, queued.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
    checked(
        &router,
        "actions/cancel-workflow-run",
        "POST",
        &cancel,
        Some(&writer),
        StatusCode::CONFLICT,
    )
    .await;
    let (foreign, _) = results::run(&f, repos[1].id, alice, "main").await;
    checked(
        &router,
        "actions/cancel-workflow-run",
        "POST",
        &format!(
            "/api/v3/repos/alice/other/actions/runs/{}/cancel",
            foreign.id
        ),
        Some(write_token),
        StatusCode::NOT_FOUND,
    )
    .await;
    let refusal = codeyard_db_actions::ci::request_ci_cancellation_with_credential(
        f.pool(),
        foreign.run_id,
        alice,
        &codeyard_database::queries::ci::CiCredential::Token {
            id: write_pat.id,
            hash: codeyard_security::token_hash::token_hash(write_token),
            permission: "actions",
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        AdmissionError::from(refusal),
        AdmissionError::Denied(_)
    ));
    assert!(
        !codeyard_db_actions::ci::get_ci_run_by_id(f.pool(), repos[1].id, foreign.id)
            .await
            .unwrap()
            .unwrap()
            .cancel_requested
    );
    let (target, runtime) = results::run(&f, repo.id, alice, "main").await;
    let target_credentials = codeyard_ci::tokens::issue(f.pool(), &target, &definitions, false)
        .await
        .unwrap();
    let target_hash = codeyard_security::token_hash::token_hash(
        &target_credentials.workflows[".github/workflows/ci.yml"]["reader"],
    );
    assert!(codeyard_db_actions::ci::append_ci_logs(
        f.pool(),
        target.id,
        "test",
        0,
        &[LogLine {
            stage: "ci".into(),
            line: "saved output".into(),
            level: "info".into(),
            ..Default::default()
        }]
    )
    .await
    .unwrap());
    let mut zip = vec![0; 22];
    zip[..4].copy_from_slice(b"PK\x05\x06");
    let artifact_id = artifact(&router, &target, &runtime.token, "dist", &zip).await;
    let cache_id = results::cache(&router, &runtime.token, "keep-cache", b"cache").await;
    for suffix in ["", "/logs"] {
        checked(
            &router,
            if suffix.is_empty() {
                "actions/delete-workflow-run"
            } else {
                "actions/delete-workflow-run-logs"
            },
            "DELETE",
            &format!("{base}/{}{suffix}", target.id),
            Some(&writer),
            StatusCode::CONFLICT,
        )
        .await;
    }
    checked(
        &router,
        "actions/force-cancel-workflow-run",
        "POST",
        &format!("{base}/{}/force-cancel", target.id),
        Some(write_token),
        StatusCode::ACCEPTED,
    )
    .await;
    assert!(
        codeyard_db_actions::ci::get_ci_run_by_id(f.pool(), repo.id, target.id)
            .await
            .unwrap()
            .unwrap()
            .cancel_requested
    );
    assert!(codeyard_db_actions::ci::finish_ci_run(
        f.pool(),
        target.id,
        "test",
        "cancelled",
        Some("Cancelled")
    )
    .await
    .unwrap()
    .is_some());
    checked(
        &router,
        "actions/force-cancel-workflow-run",
        "POST",
        &format!("{base}/{}/force-cancel", target.id),
        Some(write_token),
        StatusCode::CONFLICT,
    )
    .await;
    checked(
        &router,
        "actions/delete-workflow-run-logs",
        "DELETE",
        &format!("{base}/{}/logs", target.id),
        Some(&reader),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "actions/delete-workflow-run-logs",
        "DELETE",
        &format!("{base}/{}/logs", target.id),
        Some(&writer),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        f.scalar("SELECT log_lines FROM ci_runs WHERE id=$1", &[target.id])
            .await,
        0
    );
    assert_eq!(
        f.scalar("SELECT log_bytes FROM ci_runs WHERE id=$1", &[target.id])
            .await,
        0
    );
    assert!(
        codeyard_db_actions::ci::list_ci_logs(f.pool(), target.id, 100)
            .await
            .unwrap()
            .is_empty()
    );
    // Repository mutations must not trust a stale ingress access decision.
    f.execute(
        "INSERT INTO repo_access(repo_id,user_id,access_level) VALUES($1,$2,3)",
        &[repo.id, bob],
    )
    .await;
    f.execute(
        "DELETE FROM repo_access WHERE repo_id=$1 AND user_id=$2",
        &[repo.id, bob],
    )
    .await;
    let refusal = codeyard_db_actions::ci::delete_ci_run_data(
        f.pool(),
        bob,
        repo.id,
        target.id,
        CiRunDeletion::Run,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        AdmissionError::from(refusal),
        AdmissionError::Denied(_)
    ));
    checked(
        &router,
        "actions/delete-workflow-run",
        "DELETE",
        &format!("{base}/{}", target.id),
        Some(write_token),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert!(
        codeyard_db_actions::ci::get_ci_run_by_id(f.pool(), repo.id, target.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        codeyard_db_identity::users::tokens::find_token(f.pool(), &target_hash)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_objects WHERE id=$1",
            &[artifact_id]
        )
        .await,
        0
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_objects WHERE id=$1 AND run_id IS NULL",
            &[cache_id]
        )
        .await,
        1
    );
    checked(
        &router,
        "actions/delete-workflow-run",
        "DELETE",
        &format!("{base}/{}", target.id),
        Some(&writer),
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "actions/list-workflow-run-artifacts",
        "GET",
        &format!("{base}/{}/artifacts", target.id),
        Some(&reader),
        StatusCode::NOT_FOUND,
    )
    .await;
    f.finish().await;
}
#[tokio::test]
async fn sqlite_actions_run_mutations() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_actions_run_mutations() {
    contract(true).await;
}
