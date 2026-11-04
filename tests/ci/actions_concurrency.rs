//! Workflow leases serialize claims, replace pending runs, and retain finished-run configuration.
use super::{
    actions_objects::pat,
    actions_workflows::{checked, router},
};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use axum::http::StatusCode;
use codeyard::backend::db;
use codeyard_app::repositories::store::editor::FileWrite;
use codeyard_app::repositories::store::repo_store;
use serde_json::{json, Value};

async fn dispatch(api: &axum::Router, token: &str, workflow: &str) -> i64 {
    let (_, body) = checked(
        api,
        "actions/create-workflow-dispatch",
        "POST",
        &format!("/api/v3/repos/alice/project/actions/workflows/{workflow}/dispatches"),
        token,
        json!({"ref":"main","return_run_details":true}),
        StatusCode::OK,
    )
    .await;
    body["workflow_run_id"].as_i64().unwrap()
}
async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let mut sha = {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        crate::support::production_ci::commit(&git, "refs/heads/main", None, "Build", None)
    };
    for (file, group, cancel) in [
        ("queued.yml", "Deploy-${{ github.ref_name }}", false),
        ("cancel.yml", "DEPLOY-main", true),
        ("other.yml", "independent", false),
    ] {
        let yaml = format!("name: Deploy\non: workflow_dispatch\nconcurrency:\n  group: '{group}'\n  cancel-in-progress: {cancel}\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps: [{{run: echo ok}}]\n");
        sha = repo_store(&repo, None)
            .save_file_as(
                f.pool(),
                repo.id,
                alice,
                &FileWrite {
                    branch: "main".into(),
                    path: format!(".github/workflows/{file}"),
                    message: "Add workflow".into(),
                    expected_revision: sha,
                    creating: true,
                    deleting: false,
                },
                yaml.as_bytes(),
                "Alice",
                "alice@example.test",
            )
            .await
            .unwrap()
            .sha;
    }
    let api = router(&f, root.path(), alice).await;
    let writer = pat(&f, &api, alice, "write").await;
    let reader = pat(&f, &api, alice, "read").await;
    let outsider = pat(&f, &api, bob, "read").await;
    let base = "/api/v3/repos/alice/project/actions";
    let first = dispatch(&api, &writer, "queued.yml").await;
    let pending = dispatch(&api, &writer, "queued.yml").await;
    let newest = dispatch(&api, &writer, "queued.yml").await;
    assert_eq!(
        db::ci::get_ci_run_by_id(f.pool(), repo.id, pending)
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
    let claimed = db::ci::claim_ci_run(
        f.pool(),
        "first",
        "local",
        &[alice],
        &codeyard_app::ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claimed.id, first);
    assert!(db::ci::claim_ci_run(
        f.pool(),
        "blocked",
        "local",
        &[alice],
        &codeyard_app::ci::config::CiConfig::default().platform_labels()
    )
    .await
    .unwrap()
    .is_none());
    let independent = dispatch(&api, &writer, "other.yml").await;
    assert_eq!(
        db::ci::claim_ci_run(
            f.pool(),
            "other",
            "local",
            &[alice],
            &codeyard_app::ci::config::CiConfig::default().platform_labels()
        )
        .await
        .unwrap()
        .unwrap()
        .id,
        independent
    );
    let (_, groups) = checked(
        &api,
        "actions/list-concurrency-groups-for-repository",
        "GET",
        &format!("{base}/concurrency_groups"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(groups["total_count"], 2);
    let (headers, one) = checked(
        &api,
        "actions/list-concurrency-groups-for-repository",
        "GET",
        &format!("{base}/concurrency_groups?per_page=1"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(one["concurrency_groups"].as_array().unwrap().len(), 1);
    let link = headers["link"].to_str().unwrap();
    assert!(!link.contains("rel=\"prev\""));
    let next = url::Url::parse(link.split('<').nth(1).unwrap().split('>').next().unwrap()).unwrap();
    let (_, next) = checked(
        &api,
        "actions/list-concurrency-groups-for-repository",
        "GET",
        &format!("{}?{}", next.path(), next.query().unwrap()),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(next["concurrency_groups"][0]["group_name"], "independent");
    let (_, queue) = checked(
        &api,
        "actions/get-concurrency-group-for-repository",
        "GET",
        &format!("{base}/concurrency_groups/deploy-main?ahead_of_run={newest}"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(queue["total_count"], 2);
    assert_eq!(queue["group_members"][0]["run_id"], first);
    assert_eq!(queue["group_members"][1]["status"], "pending");
    let (_, associated) = checked(
        &api,
        "actions/list-concurrency-groups-for-workflow-run",
        "GET",
        &format!("{base}/runs/{newest}/concurrency_groups"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        associated["concurrency_groups"][0]["group_members"][0]["position"],
        1
    );
    let cancel = dispatch(&api, &writer, "cancel.yml").await;
    assert!(
        db::ci::get_ci_run_by_id(f.pool(), repo.id, first)
            .await
            .unwrap()
            .unwrap()
            .cancel_requested
    );
    assert_eq!(
        db::ci::get_ci_run_by_id(f.pool(), repo.id, newest)
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
    assert!(db::ci::claim_ci_run(
        f.pool(),
        "blocked",
        "local",
        &[alice],
        &codeyard_app::ci::config::CiConfig::default().platform_labels()
    )
    .await
    .unwrap()
    .is_none());
    db::ci::finish_ci_run(f.pool(), first, "first", "cancelled", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        db::ci::claim_ci_run(
            f.pool(),
            "replacement",
            "local",
            &[alice],
            &codeyard_app::ci::config::CiConfig::default().platform_labels()
        )
        .await
        .unwrap()
        .unwrap()
        .id,
        cancel
    );
    db::ci::finish_ci_run(f.pool(), cancel, "replacement", "success", None)
        .await
        .unwrap()
        .unwrap();
    checked(
        &api,
        "actions/get-concurrency-group-for-repository",
        "GET",
        &format!("{base}/concurrency_groups/deploy-main"),
        &reader,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let (_, retained) = checked(
        &api,
        "actions/list-concurrency-groups-for-workflow-run",
        "GET",
        &format!("{base}/runs/{first}/concurrency_groups"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(retained["total_count"], 1);
    assert_eq!(
        retained["concurrency_groups"][0]["group_members"],
        json!([])
    );
    for path in [
        format!("{base}/concurrency_groups"),
        format!("{base}/concurrency_groups/independent"),
        format!("{base}/runs/{first}/concurrency_groups"),
    ] {
        checked(
            &api,
            "",
            "GET",
            &path,
            &outsider,
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    checked(
        &api,
        "actions/get-concurrency-group-for-repository",
        "GET",
        &format!("{base}/concurrency_groups/independent?ahead_of_run={first}"),
        &reader,
        Value::Null,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &api,
        "actions/list-concurrency-groups-for-repository",
        "GET",
        &format!("{base}/concurrency_groups?after=garbage"),
        &reader,
        Value::Null,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    f.finish().await;
}
#[tokio::test]
async fn actions_concurrency_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn actions_concurrency_postgres() {
    contract(true).await;
}
