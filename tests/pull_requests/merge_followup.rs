//! An applied merge still reconciles PRs and CI when repository metadata cannot be saved.
use crate::support::{database::Database, fake_webui_bundle, http_client::Client};
use axum::http::StatusCode;
use base64::Engine;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db,
    repositories::{lifecycle, pull_requests::merge},
    storage::ObjectStore,
};
use serde_json::json;
use std::sync::Arc;

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut database = Database::new(pg).await;
    database.complete_setup().await;
    let actor = database.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (database.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let state = AppState::new(
        database.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig {
            trusted_actor_ids: [actor].into(),
            ..Default::default()
        },
        None,
    );
    let alice = Client::login(&create_router(state.clone()), database.pool(), actor).await;
    let base = alice.save("main", "README", "unborn").await;
    alice.branch("feature", &base).await;
    let workflow = "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: []\n";
    let saved = alice.expect("POST", "/api/repos/alice/proj/contents/.github/workflows/check.yml",
        json!({"branch":"feature", "expected_revision":base, "message":"workflow", "content":base64::engine::general_purpose::STANDARD.encode(workflow)}), StatusCode::CREATED).await;
    let head = saved["sha"].as_str().unwrap();
    let pull = alice
        .open_pull("Merge despite metadata error", "feature", "main")
        .await;
    let number = pull["number"].as_i64().unwrap() as i32;
    let prepared = merge::prepare_pull_request_merge(
        database.pool(),
        &state.ci_runs,
        actor,
        ("alice".into(), "proj".into(), number),
        merge::MergeRequest {
            expected_head: head.into(),
            expected_base: base,
            expected_base_branch: None,
            expected_target: None,
            merge_method: Default::default(),
        },
    )
    .await
    .unwrap();
    if pg {
        database.execute("CREATE FUNCTION fail_repo_metadata() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'metadata unavailable'; END $$", &[]).await;
        database.execute("CREATE TRIGGER fail_repo_metadata BEFORE UPDATE OF default_branch ON repositories FOR EACH ROW EXECUTE FUNCTION fail_repo_metadata()", &[]).await;
    } else {
        database.execute("CREATE TRIGGER fail_repo_metadata BEFORE UPDATE OF default_branch ON repositories BEGIN SELECT RAISE(FAIL, 'metadata unavailable'); END", &[]).await;
    }
    let result = merge::publish_prepared_merge(
        database.pool(),
        &state.ci_runs,
        &state.notification_broadcaster,
        prepared,
    )
    .await
    .unwrap();
    assert!(result.merged);
    assert!(result.message.contains("bookkeeping failed"));
    let merged = alice.pull(number as i64).await;
    assert_eq!(merged["state"], "merged");
    let bare = git2::Repository::open_bare(&repo.disk_path).unwrap();
    assert_eq!(
        bare.refname_to_id("refs/heads/main").unwrap().to_string(),
        result.sha
    );
    let runs = db::ci::list_ci_runs_for_repo(database.pool(), repo.id, 100)
        .await
        .unwrap();
    assert!(runs
        .iter()
        .any(|run| run.event == "push" && run.head_sha.as_deref() == Some(&result.sha)));
    if pg {
        database
            .execute("DROP TRIGGER fail_repo_metadata ON repositories", &[])
            .await;
        database
            .execute("DROP FUNCTION fail_repo_metadata()", &[])
            .await;
    } else {
        database
            .execute("DROP TRIGGER fail_repo_metadata", &[])
            .await;
    }
    database.finish().await;
}

#[tokio::test]
async fn sqlite_applied_merge_finishes_followups_after_metadata_failure() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_applied_merge_finishes_followups_after_metadata_failure() {
    contract(true).await;
}
