//! Fork runs remain unclaimable until a trusted base writer approves the pinned head.
use super::{
    actions_objects::pat,
    actions_workflows::{checked, router},
};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use axum::http::StatusCode;
use codeyard::backend::db;
use codeyard_app::repositories::lifecycle;
use serde_json::{json, Value};

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
        "project",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let main = {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        crate::support::production_ci::commit(&git, "refs/heads/main", None, "Base", None)
    };
    let fork = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(bob),
        "project",
        None,
        false,
        Some(&repo),
    )
    .await
    .unwrap();
    let head = {
        let git = git2::Repository::open_bare(&fork.disk_path).unwrap();
        crate::support::production_ci::commit(&git, "refs/heads/main", Some(&main), "Fork", None)
    };
    let api = router(&f, root.path(), alice).await;
    let writer = pat(&f, &api, alice, "write").await;
    let reader = pat(&f, &api, alice, "read").await;
    let outsider = pat(&f, &api, bob, "write").await;
    checked(
        &api,
        "pulls/create",
        "POST",
        "/api/v3/repos/alice/project/pulls",
        &outsider,
        json!({"title":"Fork changes","head":"bob:main","base":"main"}),
        StatusCode::CREATED,
    )
    .await;
    let runs = codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    let pending = &runs[0];
    assert_eq!(pending.head_sha.as_deref(), Some(head.as_str()));
    assert!(pending.is_fork && pending.approval_required);
    assert!(codeyard_db_actions::ci::claim_ci_run(
        f.pool(),
        "blocked",
        "local",
        &[alice, bob],
        &codeyard_app::ci::config::CiConfig::default().platform_labels()
    )
    .await
    .unwrap()
    .is_none());
    let url = format!("/api/v3/repos/alice/project/actions/runs/{}", pending.id);
    let (_, shown) = checked(
        &api,
        "actions/get-workflow-run",
        "GET",
        &url,
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(shown["status"], "waiting");
    let approve = format!("{url}/approve");
    checked(
        &api,
        "actions/approve-workflow-run",
        "POST",
        &approve,
        &reader,
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &api,
        "actions/approve-workflow-run",
        "POST",
        &approve,
        &outsider,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let (_, approved) = checked(
        &api,
        "actions/approve-workflow-run",
        "POST",
        &approve,
        &writer,
        Value::Null,
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(approved, serde_json::json!({}));
    let released = codeyard_db_actions::ci::claim_ci_run(
        f.pool(),
        "approved",
        "local",
        &[alice],
        &codeyard_app::ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(released.id, pending.id);
    assert_eq!(released.triggered_by_id, Some(alice));
    assert!(released.is_fork && !released.approval_required);
    checked(
        &api,
        "actions/approve-workflow-run",
        "POST",
        &approve,
        &writer,
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    assert_eq!(
        codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
            .await
            .unwrap()
            .len(),
        1
    );
    f.finish().await;
}
#[tokio::test]
async fn actions_approval_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn actions_approval_postgres() {
    contract(true).await;
}
