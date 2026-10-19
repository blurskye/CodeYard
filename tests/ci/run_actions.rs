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
    assert!(db::ci::has_ci_accepted_push(
        f.pool(),
        repo.id,
        alice_id,
        "refs/heads/main",
        run.head_sha.as_deref().unwrap()
    )
    .await
    .unwrap());
    let claimed = db::ci::claim_ci_run(f.pool(), "log-worker", "local", &[alice_id, bob_id])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, run.id);
    let output = [
        "failed to pull act-latest",
        "time=\"now\" level=error msg=\"application failure\"",
    ];
    db::ci::append_ci_logs(
        f.pool(),
        run.id,
        "log-worker",
        &output
            .iter()
            .map(|line| codeyard::backend::ci::queue::LogLine {
                stage: "ci.yml".into(),
                line: (*line).into(),
                level: "info".into(),
            })
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    let api = alice
        .expect(
            "GET",
            &format!("/api/ci/jobs/{}/logs", run.run_id),
            serde_json::Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(api["logs"].as_array().unwrap().len(), output.len());
    let job = run.run_id.to_string();
    let cancel = format!("/alice/proj/actions/runs/{job}/cancel");
    let rerun = format!("/alice/proj/actions/runs/{job}/rerun");

    // The run page offers the cancel form to a writer, and nothing to a reader.
    let (_, page) = alice.page(&format!("/alice/proj/actions?job={job}")).await;
    assert!(page.contains(&format!("action=\"{cancel}\"")), "{page}");
    let (_, page) = dave.page(&format!("/alice/proj/actions?job={job}")).await;
    assert!(!page.contains(&format!("action=\"{cancel}\"")), "{page}");

    // Global selections authorize before redirecting to the canonical run controls.
    let global = alice
        .exchange(
            "GET",
            &format!("/ci?job={job}&view=logs&log_q=failed"),
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(global.status, StatusCode::SEE_OTHER);
    assert_eq!(
        global.location.as_deref(),
        Some(format!("/alice/proj/actions?job={job}&log_q=failed&view=logs").as_str())
    );

    // An unreadable repository and a missing one have the same run-action refusal.
    let private = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "private",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    for action in ["cancel", "rerun"] {
        let random = uuid::Uuid::new_v4();
        for name in ["missing", "private"] {
            let reply = dave
                .form_reply(
                    &format!("/alice/{name}/actions/runs/{random}/{action}"),
                    &[],
                )
                .await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND);
            assert!(reply.text().contains("Repository not found."));
            assert!(!reply.text().contains("Run not found in this repository."));
        }
    }
    // A private global selection must not disclose its canonical repository URL.
    f.execute(
        "UPDATE ci_runs SET repo_id=$1 WHERE id=$2",
        &[private.id, run.id],
    )
    .await;
    let hidden = dave
        .exchange("GET", &format!("/ci?job={job}"), "text/html", Vec::new())
        .await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);
    assert!(hidden.location.is_none());
    f.execute(
        "UPDATE ci_runs SET repo_id=$1 WHERE id=$2",
        &[repo.id, run.id],
    )
    .await;

    // Failure to discover current workflows leaves persisted history, logs and cancellation intact.
    let (status, model) = alice
        .model(&format!("/alice/proj/actions?job={job}&ref=missing"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let codeyard::webui::app::Page::Actions(codeyard::webui::pages::ci::Page::Actions(model)) =
        model
    else {
        panic!("Actions model")
    };
    assert!(model.workflow_warning.is_some());
    assert!(!model.can_dispatch);
    assert!(model.can_manage_runs);
    assert_eq!(
        model
            .selected
            .as_ref()
            .unwrap()
            .logs
            .iter()
            .map(|line| line.line.as_str())
            .collect::<Vec<_>>(),
        output
    );
    let (status, page) = alice
        .page(&format!("/alice/proj/actions?job={job}&ref=missing"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(&format!("action=\"{cancel}\"")));

    // A reader cannot re-run or cancel, and a writer who did not start the run cannot cancel it.
    let (status, page) = dave.form(&rerun, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = dave.form(&cancel, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = bob.form(&cancel, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");

    // Another writer can re-run a manual commit displaced by an unrelated branch reset.
    {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        crate::support::production_ci::commit(&git, "refs/heads/main", None, "unrelated", None);
    }
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
    assert!(
        db::ci::get_ci_run_by_run_id(f.pool(), run.run_id)
            .await
            .unwrap()
            .unwrap()
            .cancel_requested
    );
    db::ci::finish_ci_run(f.pool(), run.id, "log-worker", "cancelled", None)
        .await
        .unwrap()
        .unwrap();
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

/// A pull request run is run again from its page as a new attempt at the same head commit. A
/// reader, and a writer who may not start runs, are refused.
async fn pull_request_run_is_run_again(pg: bool) {
    use base64::Engine;

    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let carol_id = f.actor("carol").await;
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
    for writer in [bob_id, carol_id] {
        db::repositories::access::grant_repo_access(f.pool(), writer, repo.id, AccessLevel::Write)
            .await
            .unwrap();
    }
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig {
            // Bob may start runs. Carol may write to the repository but may not start runs.
            trusted_actor_ids: BTreeSet::from([alice_id, bob_id]),
            ..CiConfig::default()
        },
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let carol = Client::login(&router, f.pool(), carol_id).await;
    let dave = Client::login(&router, f.pool(), dave_id).await;

    // A workflow for pull requests on main, and a topic branch whose head carries it.
    let content = base64::engine::general_purpose::STANDARD.encode(
        "name: Review\non: pull_request\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo test\n",
    );
    let main = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/contents/.github/workflows/ci.yml",
            json!({"content": content, "message": "Workflow", "branch": "main", "expected_revision": "unborn"}),
            StatusCode::CREATED,
        )
        .await;
    let main = main["sha"].as_str().unwrap().to_owned();
    alice.branch("topic", &main).await;
    let head = alice.save("topic", "topic.txt", &main).await;
    alice.open_pull("Review me", "topic", "main").await;

    // Opening the pull request queued its workflow at the head commit.
    let first = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|run| run.event == "pull_request")
        .expect("opening the pull request queues its workflow");
    assert_eq!(
        (first.attempt, first.head_sha.as_deref()),
        (1, Some(head.as_str()))
    );
    let job = first.run_id.to_string();
    let rerun = format!("/alice/proj/actions/runs/{job}/rerun");

    // The run fails on its worker, which then records the result.
    let worker = "node/slot-1";
    let claimed = db::ci::claim_ci_run(f.pool(), worker, "local", &[alice_id, bob_id])
        .await
        .unwrap()
        .expect("the run is claimed");
    assert_eq!(claimed.id, first.id);
    db::ci::finish_ci_run(
        f.pool(),
        first.id,
        worker,
        "failed",
        Some("A workflow job failed"),
    )
    .await
    .unwrap()
    .expect("the run is finished");

    // Another open PR may share its SHA without sharing its identity or approval.
    alice.branch("same-head", &head).await;
    alice.open_pull("Another review", "same-head", "main").await;

    // The run page offers Re-run for the finished pull request run.
    let (_, page) = alice.page(&format!("/alice/proj/actions?job={job}")).await;
    assert!(page.contains(&format!("action=\"{rerun}\"")), "{page}");

    // A reader cannot re-run it, and a writer who may not start runs is refused.
    let (status, page) = dave.form(&rerun, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = carol.form(&rerun, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    assert_eq!(
        db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
            .await
            .unwrap()
            .len(),
        2
    );

    // A trusted writer starts attempt 2 at the same head commit, and asking again while it is
    // queued returns that attempt rather than a third one.
    let reply = bob.form_reply(&rerun, &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 3, "{runs:?}");
    let again = runs
        .iter()
        .find(|candidate| {
            candidate.run_id != first.run_id && candidate.pull_request_id == first.pull_request_id
        })
        .unwrap();
    assert_eq!(again.attempt, 2);
    assert_eq!(again.status, "queued");
    assert_eq!(again.event, "pull_request");
    assert_eq!(again.head_sha, first.head_sha);
    assert_eq!(again.pull_request_id, first.pull_request_id);
    assert_eq!(again.triggered_by_id, Some(bob_id));
    let reply = bob.form_reply(&rerun, &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(
        db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
            .await
            .unwrap()
            .len(),
        3
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_pull_request_run_is_run_again() {
    pull_request_run_is_run_again(true).await;
}

#[tokio::test]
async fn sqlite_pull_request_run_is_run_again() {
    pull_request_run_is_run_again(false).await;
}
