//! Cancelling and re-running a CI run from its page, through the native forms. Writers may use
//! them, readers may not, and a run can be cancelled by the person who started it. On SQLite and
//! PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database::Database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client::Client;
use axum::http::StatusCode;
use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;

use codeyard_db_core::models::AccessLevel;
use codeyard_storage::ObjectStore;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

async fn contract(pg: bool) {
    use base64::Engine;

    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let dave_id = f.actor("dave").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob_id,
        repo.id,
        AccessLevel::Write,
    )
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
    let run = codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("the dispatched run");
    assert!(codeyard_db_actions::ci::has_ci_accepted_push(
        f.pool(),
        repo.id,
        alice_id,
        "refs/heads/main",
        run.head_sha.as_deref().unwrap()
    )
    .await
    .unwrap());
    let claimed = codeyard_db_actions::ci::claim_ci_run(
        f.pool(),
        "log-worker",
        "local",
        &[alice_id, bob_id],
        &codeyard_ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claimed.id, run.id);
    let output = [
        "failed to pull act-latest",
        "time=\"now\" level=error msg=\"application failure\"",
    ];
    codeyard_db_actions::ci::append_ci_logs(
        f.pool(),
        run.id,
        "log-worker",
        0,
        &output
            .iter()
            .map(|line| codeyard_model::ci::LogLine {
                stage: "ci.yml".into(),
                line: (*line).into(),
                level: "info".into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    let api = alice
        .expect(
            "GET",
            &format!(
                "/api/v3/repos/alice/proj/actions/runs/{}/logs?after=0",
                run.id
            ),
            serde_json::Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(api["logs"].as_array().unwrap().len(), output.len());
    browser_logs(&f, &router, &alice, &dave, &run).await;
    let job = run.run_id.to_string();
    let cancel = format!("/alice/proj/actions/runs/{job}/cancel");
    let rerun = format!("/alice/proj/actions/runs/{job}/rerun");

    // The run page offers the cancel form to every writer, and nothing to a reader.
    let (_, page) = alice.page(&format!("/alice/proj/actions?job={job}")).await;
    assert!(page.contains(&format!("action=\"{cancel}\"")), "{page}");
    let page = dave
        .page_ok(&format!("/alice/proj/actions?job={job}"))
        .await;
    assert!(!page.contains(&format!("action=\"{cancel}\"")), "{page}");
    let page = bob.page_ok(&format!("/alice/proj/actions?job={job}")).await;
    assert!(page.contains(&format!("action=\"{cancel}\"")), "{page}");
    let logs = codeyard_db_actions::ci::list_ci_logs(f.pool(), run.id, 10)
        .await
        .unwrap();
    let incremental = alice
        .expect(
            "GET",
            &format!(
                "/api/v3/repos/alice/proj/actions/runs/{}/logs?after={}",
                run.id,
                logs.last().unwrap().id
            ),
            serde_json::Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(incremental["logs"], json!([]));
    assert_eq!(incremental["progress"]["status"], "running");

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
    let private = codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice_id),
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
    let codeyard_webui::app::Page::Actions(codeyard_webui::pages::ci::Page::Actions(model)) = model
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

    // Readers cannot re-run or cancel; any repository writer may cancel.
    let (status, page) = dave.form(&rerun, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = dave.form(&cancel, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = bob.form(&cancel, &[]).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");

    // Another writer can re-run a manual commit displaced by an unrelated branch reset.
    {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        crate::support::production_ci::commit(&git, "refs/heads/main", None, "unrelated", None);
    }
    // A writer re-runs the same workflow at the same commit: a second queued run appears.
    let reply = bob.form_reply(&rerun, &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let runs = codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
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

    // Archiving disables execution, not the starter/admin's cancellation authority.
    f.execute(
        "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
        &[repo.id],
    )
    .await;
    let (_, page) = alice.page(&format!("/alice/proj/actions?job={job}")).await;
    assert!(page.contains(&format!("action=\"{cancel}\"")), "{page}");
    let reply = alice.form_reply(&cancel, &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert!(
        codeyard_db_actions::ci::get_ci_run_by_run_id(f.pool(), run.run_id)
            .await
            .unwrap()
            .unwrap()
            .cancel_requested
    );
    codeyard_db_actions::ci::finish_ci_run(f.pool(), run.id, "log-worker", "cancelled", None)
        .await
        .unwrap()
        .unwrap();
    let cancelled = codeyard_db_actions::ci::get_ci_run_by_run_id(f.pool(), run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.status, "cancelled");

    // Filtering precedes the history limit, and keyset links reach older matches.
    f.execute(
        "UPDATE repositories SET is_archived=FALSE WHERE id=$1",
        &[repo.id],
    )
    .await;
    for _ in 0..205 {
        codeyard_db_actions::ci::enqueue_ci_run(
            f.pool(),
            codeyard_db_actions::ci::NewCiRun {
                repo_id: repo.id,
                triggered_by_id: alice_id,
                pipeline: "ci",
                event: "workflow_dispatch",
                pull_request_id: None,
                branch: "main",
                head_sha: run.head_sha.as_deref().unwrap(),
                workflow_path: ".github/workflows/new.yml",
                workflow_blobs: &json!({}),
                workflow_inputs: None,
                environment: None,
                manual: true,
            },
            &[alice_id, bob_id],
        )
        .await
        .unwrap();
    }
    let url = "/alice/proj/actions?workflow=.github%2Fworkflows%2Fci.yml&branch=main&event=workflow_dispatch&limit=1";
    let (_, page) = alice.model(url).await;
    let codeyard_webui::app::Page::Actions(codeyard_webui::pages::ci::Page::Actions(page)) = page
    else {
        panic!("Actions model")
    };
    assert_eq!(page.runs.len(), 1);
    assert_eq!(page.runs[0].id, again.run_id.to_string());
    let before = page.next_before.unwrap();
    let (_, page) = alice
        .model(&format!(
            "{url}&before={before}&actor=alice&status=cancelled&q={}",
            &run.head_sha.as_deref().unwrap()[..7]
        ))
        .await;
    let codeyard_webui::app::Page::Actions(codeyard_webui::pages::ci::Page::Actions(page)) = page
    else {
        panic!("Actions model")
    };
    assert_eq!(page.runs.len(), 1);
    assert_eq!(page.runs[0].id, job);
    assert!(page.next_before.is_none());
    let (_, page) = alice.model(&format!("/alice/proj/actions?q={job}")).await;
    let codeyard_webui::app::Page::Actions(codeyard_webui::pages::ci::Page::Actions(page)) = page
    else {
        panic!("Actions model")
    };
    assert_eq!(page.runs.len(), 1);
    assert_eq!(page.runs[0].id, job);
    match &f {
        Database::Postgres(f) => {
            let definition: String = sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE schemaname=current_schema() AND indexname='idx_ci_deployments_run'").fetch_one(&f.raw).await.unwrap();
            assert!(definition.contains("(run_id)"));
        }
        Database::Sqlite(f) => {
            let definition: String = sqlx::query_scalar(
                "SELECT sql FROM sqlite_master WHERE name='idx_ci_deployments_run'",
            )
            .fetch_one(&f.raw)
            .await
            .unwrap();
            assert!(definition.contains("(run_id)"));
        }
    }
    f.finish().await;
}

async fn browser_logs(
    f: &Database,
    router: &axum::Router,
    alice: &Client,
    dave: &Client,
    run: &codeyard_db_core::models::CiRun,
) {
    use axum::{body::Body, http::Request};

    use codeyard_identity::tokens;
    use codeyard_identity::tokens::TokenInput;
    use tower::ServiceExt;

    let path = format!("/api/v3/repos/alice/proj/actions/runs/{}/logs", run.id);
    let first = alice
        .expect(
            "GET",
            &format!("{path}?after=0&limit=1"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(first["logs"].as_array().unwrap().len(), 1);
    assert_eq!(first["has_more"], true);
    let cursor = first["next_cursor"].as_i64().unwrap();
    let second = alice
        .expect(
            "GET",
            &format!("{path}?after={cursor}&limit=1"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(second["logs"].as_array().unwrap().len(), 1);
    assert!(second["logs"][0]["id"].as_i64().unwrap() > cursor);
    assert_eq!(second["has_more"], false);
    let pinned = path.replace("/logs", "/attempts/1/logs");
    let attempt = alice
        .expect(
            "GET",
            &format!("{pinned}?after=0"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(attempt["job_id"], run.run_id.to_string());
    alice
        .expect(
            "GET",
            &format!(
                "{}?after=0",
                pinned.replace("/attempts/1/", "/attempts/99/")
            ),
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    let ended = Client::login(router, f.pool(), run.triggered_by_id.unwrap()).await;
    codeyard_db_identity::users::sessions::delete_session(f.pool(), &ended.session)
        .await
        .unwrap();
    ended
        .expect(
            "GET",
            &format!("{path}?after=0"),
            Value::Null,
            StatusCode::UNAUTHORIZED,
        )
        .await;
    alice
        .expect(
            "GET",
            &format!("{path}?after=-1"),
            Value::Null,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    Client::anonymous(router)
        .expect(
            "GET",
            &format!("{path}?after=0"),
            Value::Null,
            StatusCode::UNAUTHORIZED,
        )
        .await;
    alice
        .expect(
            "GET",
            &path.replace(&format!("/runs/{}/", run.id), "/runs/9223372036854775807/"),
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    alice
        .expect(
            "GET",
            &path.replace("/proj/", "/missing/"),
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    alice
        .expect("DELETE", &path, Value::Null, StatusCode::UNAUTHORIZED)
        .await;

    // Normal browser downloads still follow the standard short-lived ZIP redirect.
    let download = alice
        .exchange("GET", &path, "application/json", Vec::new())
        .await;
    assert_eq!(download.status, StatusCode::FOUND, "{}", download.text());
    let location = download.location.unwrap();
    let location = url::Url::parse(&location)
        .map(|url| format!("{}?{}", url.path(), url.query().unwrap()))
        .unwrap_or(location);
    let archive = Client::anonymous(router)
        .exchange("GET", &location, "application/zip", Vec::new())
        .await;
    assert_eq!(archive.status, StatusCode::OK, "{}", archive.text());
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive.body)).unwrap();
    let mut text = String::new();
    for index in 0..zip.len() {
        use std::io::Read;
        zip.by_index(index)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
    }
    assert!(text.contains("failed to pull act-latest"), "{text}");

    f.execute(
        "UPDATE repositories SET is_private=TRUE WHERE id=$1",
        &[run.repo_id],
    )
    .await;
    dave.expect(
        "GET",
        &format!("{path}?after=0"),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    alice
        .expect(
            "GET",
            &format!("{path}?after=0"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    alice
        .expect(
            "GET",
            &format!("/api/v3/repos/alice/proj/actions/runs/{}", run.id),
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    let reader = tokens::create(
        f.pool(),
        run.triggered_by_id.unwrap(),
        Some(&alice.session),
        TokenInput {
            name: "browser log read",
            scope: "read",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let outsider = codeyard_db_identity::users::sessions::get_session_user(f.pool(), &dave.session)
        .await
        .unwrap()
        .unwrap();
    let outsider = tokens::create(
        f.pool(),
        outsider.id,
        Some(&dave.session),
        TokenInput {
            name: "outside log read",
            scope: "read",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    for (credential, expected) in [
        ("invalid", StatusCode::UNAUTHORIZED),
        (outsider.value.as_str(), StatusCode::NOT_FOUND),
        (reader.value.as_str(), StatusCode::OK),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("{path}?after=0"))
                    .header("cookie", format!("session_id={}", alice.session))
                    .header("authorization", format!("Bearer {credential}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    f.execute(
        "UPDATE repositories SET is_private=FALSE WHERE id=$1",
        &[run.repo_id],
    )
    .await;
    for (column, expected) in [
        ("is_banned", StatusCode::UNAUTHORIZED),
        ("must_change_password", StatusCode::PRECONDITION_REQUIRED),
    ] {
        f.execute(
            &format!("UPDATE users SET {column}=TRUE WHERE id=$1"),
            &[run.triggered_by_id.unwrap()],
        )
        .await;
        alice
            .expect("GET", &format!("{path}?after=0"), Value::Null, expected)
            .await;
        f.execute(
            &format!("UPDATE users SET {column}=FALSE WHERE id=$1"),
            &[run.triggered_by_id.unwrap()],
        )
        .await;
    }
    for suffix in ["logs", "history", "log-download", "deployment", "cancel"] {
        let method = if suffix == "cancel" { "POST" } else { "GET" };
        let reply = alice
            .exchange(
                method,
                &format!("/api/ci/jobs/{}/{suffix}", run.run_id),
                "application/json",
                Vec::new(),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{suffix}: {}",
            reply.text()
        );
    }
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
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let carol_id = f.actor("carol").await;
    let dave_id = f.actor("dave").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    for writer in [bob_id, carol_id] {
        codeyard_db_repos::repositories::access::grant_repo_access(
            f.pool(),
            writer,
            repo.id,
            AccessLevel::Write,
        )
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
    let first = codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
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
    let claimed = codeyard_db_actions::ci::claim_ci_run(
        f.pool(),
        worker,
        "local",
        &[alice_id, bob_id],
        &codeyard_ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .expect("the run is claimed");
    assert_eq!(claimed.id, first.id);
    codeyard_db_actions::ci::finish_ci_run(
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

    let page = carol
        .page_ok(&format!("/alice/proj/actions?job={job}"))
        .await;
    assert!(!page.contains(&format!("action=\"{rerun}\"")), "{page}");
    let page = carol.page_ok("/alice/proj/actions").await;
    assert!(!page.contains("Run workflow"), "{page}");

    // A reader cannot re-run it, and a writer who may not start runs is refused.
    let (status, page) = dave.form(&rerun, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = carol.form(&rerun, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    assert_eq!(
        codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
            .await
            .unwrap()
            .len(),
        2
    );

    // A trusted writer starts attempt 2 at the same head commit, and asking again while it is
    // queued returns that attempt rather than a third one.
    let reply = bob.form_reply(&rerun, &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let runs = codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
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
    assert_eq!(first.workflow_run_id, Some(first.id));
    assert_eq!(again.workflow_run_id, Some(first.id));
    assert_eq!(first.workflow_run_attempt, Some(1));
    assert_eq!(again.workflow_run_attempt, Some(2));
    assert_eq!(again.workflow_run_number, first.workflow_run_number);
    let latest = codeyard_db_actions::action_runs::get(f.pool(), repo.id, first.id, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.id, again.id);
    assert_eq!(
        codeyard_db_actions::action_runs::get(f.pool(), repo.id, first.id, Some(1))
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    assert_eq!(
        codeyard_db_actions::action_runs::get(f.pool(), repo.id, first.id, Some(2))
            .await
            .unwrap()
            .unwrap()
            .id,
        again.id
    );
    assert!(
        codeyard_db_actions::action_runs::get(f.pool(), repo.id, first.id, Some(3))
            .await
            .unwrap()
            .is_none()
    );
    let token = super::actions_objects::pat(&f, &router, alice_id, "write").await;
    let api_base = "/api/v3/repos/alice/proj/actions/runs";
    let (_, document) = super::actions_workflows::checked(
        &router,
        "actions/get-workflow-run",
        "GET",
        &format!("{api_base}/{}", first.id),
        &token,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(document["id"], first.id);
    assert_eq!(document["run_attempt"], 2);
    assert_eq!(document["actor"]["login"], "alice");
    assert_eq!(document["triggering_actor"]["login"], "bob");
    assert_eq!(document["head_branch"], "topic");
    assert_eq!(
        document["pull_requests"][0]["id"],
        first.pull_request_id.unwrap()
    );
    let (_, list) = super::actions_workflows::checked(
        &router,
        "actions/list-workflow-runs-for-repo",
        "GET",
        &format!("{api_base}?actor=alice"),
        &token,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(list["total_count"], 2);
    let (_, list) = super::actions_workflows::checked(
        &router,
        "actions/list-workflow-runs-for-repo",
        "GET",
        &format!("{api_base}?actor=bob"),
        &token,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(list["total_count"], 0);
    super::actions_workflows::checked(
        &router,
        "",
        "GET",
        &format!("{api_base}/{}", again.id),
        &token,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    // A terminal first attempt cannot be used to delete a live later attempt or its logs.
    for suffix in ["", "/logs"] {
        super::actions_workflows::checked(
            &router,
            "",
            "DELETE",
            &format!("{api_base}/{}{suffix}", first.id),
            &token,
            Value::Null,
            StatusCode::CONFLICT,
        )
        .await;
    }
    let reply = bob.form_reply(&rerun, &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(
        codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
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
