//! Cancelling and re-running a CI run from its page, through the native forms. Writers may use
//! them, readers may not, and a run can be cancelled by the person who started it. On SQLite and
//! PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database::Database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests::Client;
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, db::models::AccessLevel,
    storage::ObjectStore,
};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;

async fn contract(pg: bool) {
    use base64::Engine;

    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let dave_id = f.actor("dave").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(f.pool(), bob_id, repo.id, AccessLevel::Write)
        .await
        .unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig {
            // Only trusted actors may start runs, so bob is trusted to re-run one.
            trusted_actor_ids: BTreeSet::from([alice_id, bob_id]),
            ..CiConfig::default()
        },
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let dave = Client::login(&router, f.pool(), dave_id).await;

    let content = base64::engine::general_purpose::STANDARD.encode(
        "name: Manual\non: workflow_dispatch\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo test\n",
    );
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/contents/.github/workflows/ci.yml",
            json!({"content": content, "message": "Workflow", "branch": "main", "expected_revision": "unborn"}),
            StatusCode::CREATED,
        )
        .await;
    let reply = alice
        .form_reply(
            "/alice/proj/actions/dispatch",
            &[
                ("branch", "main"),
                ("workflow_path", ".github/workflows/ci.yml"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let run = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("the dispatched run");
    let job = run.run_id.to_string();
    let cancel = format!("/alice/proj/actions/runs/{job}/cancel");
    let rerun = format!("/alice/proj/actions/runs/{job}/rerun");

    // The run page offers the cancel form to a writer, and nothing to a reader.
    let (_, page) = alice.page(&format!("/alice/proj/actions?job={job}")).await;
    assert!(page.contains(&format!("action=\"{cancel}\"")), "{page}");
    let (_, page) = dave.page(&format!("/alice/proj/actions?job={job}")).await;
    assert!(!page.contains(&format!("action=\"{cancel}\"")), "{page}");

    // A reader cannot re-run or cancel, and a writer who did not start the run cannot cancel it.
    let (status, page) = dave.form(&rerun, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = dave.form(&cancel, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = bob.form(&cancel, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");

    // A writer re-runs the same workflow at the same commit: a second queued run appears.
    let reply = bob.form_reply(&rerun, &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 2, "{runs:?}");
    let again = runs
        .iter()
        .find(|candidate| candidate.run_id != run.run_id)
        .unwrap();
    assert_eq!(again.head_sha, run.head_sha);
    assert_eq!(again.workflow_path, run.workflow_path);
    assert_eq!(again.status, "queued");

    // The person who started the first run cancels it.
    let reply = alice.form_reply(&cancel, &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let cancelled = db::ci::get_ci_run_by_run_id(f.pool(), run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.status, "cancelled");
    f.finish().await;
}

#[tokio::test]
async fn postgres_cancel_and_rerun_from_the_run_page() {
    contract(true).await;
}

#[tokio::test]
async fn sqlite_cancel_and_rerun_from_the_run_page() {
    contract(false).await;
}
