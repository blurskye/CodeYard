//! Native-backed run documents, stable workflow counters, filters and attempt lookup.
use super::{
    actions_objects::pat,
    actions_workflows::{checked, router},
};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use axum::http::StatusCode;

use codeyard_repos::editor::FileWrite;
use codeyard_repos_core::repositories::store::repo_store;
use serde_json::{json, Value};

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repos = root.path().join("repos");
    let repo = codeyard_repos_core::repositories::lifecycle::provision(
        (f.pool(), None),
        &repos,
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let sha = {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        crate::support::production_ci::commit(&git, "refs/heads/main", None, "Build", None)
    };
    let store = repo_store(&repo, None);
    store.save_file_as(f.pool(),repo.id,alice,&FileWrite{branch:"main".into(),path:".github/workflows/other.yml".into(),message:"Add another workflow".into(),expected_revision:sha,creating:true,deleting:false},b"name: Other\non: workflow_dispatch\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n","Alice","alice@example.test").await.unwrap();
    let head = store.commit("main").await.unwrap();
    let api = router(&f, &repos, alice).await;
    let writer = pat(&f, &api, alice, "write").await;
    let reader = pat(&f, &api, alice, "read").await;
    let outsider = pat(&f, &api, bob, "write").await;
    let base = "/api/v3/repos/alice/project/actions";
    let mut ids = Vec::new();
    for workflow in ["ci.yml", "other.yml", "ci.yml"] {
        let (_, body) = checked(
            &api,
            "actions/create-workflow-dispatch",
            "POST",
            &format!("{base}/workflows/{workflow}/dispatches"),
            &writer,
            json!({"ref":"main","return_run_details":true}),
            StatusCode::OK,
        )
        .await;
        ids.push(body["workflow_run_id"].as_i64().unwrap());
    }
    let first = codeyard_db_actions::ci::claim_ci_run(
        f.pool(),
        "documents",
        "local",
        &[alice],
        &codeyard_ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.id, ids[0]);
    codeyard_db_actions::ci::finish_ci_run(
        f.pool(),
        first.id,
        "documents",
        "failed",
        Some("Build failed"),
    )
    .await
    .unwrap()
    .unwrap();
    let running = codeyard_db_actions::ci::claim_ci_run(
        f.pool(),
        "documents",
        "local",
        &[alice],
        &codeyard_ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(running.id, ids[1]);
    let mut workflow = 0;
    for (index, number, status, conclusion) in [
        (0, 1, "completed", json!("failure")),
        (1, 1, "in_progress", Value::Null),
        (2, 2, "queued", Value::Null),
    ] {
        let path = format!("{base}/runs/{}?exclude_pull_requests=true", ids[index]);
        let (_, body) = checked(
            &api,
            "actions/get-workflow-run",
            "GET",
            &path,
            &reader,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(body["id"], ids[index]);
        assert_eq!(body["run_number"], number);
        assert_eq!(body["run_attempt"], 1);
        assert_eq!(body["name"], if index == 1 { "Other" } else { "Build" });
        assert_eq!(body["event"], "workflow_dispatch");
        for field in ["created_at", "updated_at"] {
            chrono::DateTime::parse_from_rfc3339(body[field].as_str().unwrap()).unwrap();
        }
        if index == 2 {
            assert!(body["run_started_at"].is_null());
        } else {
            chrono::DateTime::parse_from_rfc3339(body["run_started_at"].as_str().unwrap()).unwrap();
        }
        let jobs = url::Url::parse(body["jobs_url"].as_str().unwrap()).unwrap();
        assert_eq!(jobs.path(), format!("{base}/runs/{}/jobs", ids[index]));
        let html = url::Url::parse(body["html_url"].as_str().unwrap()).unwrap();
        assert_eq!(html.path(), "/alice/project/actions");
        assert!(html.query().unwrap().starts_with("job="));
        assert_eq!(body["head_repository"]["name"], "project");
        assert_eq!(body["head_repository"]["owner"]["login"], "alice");
        assert_eq!(body["status"], status);
        assert_eq!(body["conclusion"], conclusion);
        assert_eq!(body["head_sha"], head.sha);
        assert_eq!(body["head_branch"], "main");
        assert_eq!(body["head_commit"]["id"], head.sha);
        assert_eq!(body["head_commit"]["tree_id"], head.tree_sha);
        assert_eq!(body["head_commit"]["message"], head.message);
        assert_eq!(body["display_title"], head.message.lines().next().unwrap());
        assert_eq!(body["repository"]["id"], repo.id);
        assert_eq!(body["head_repository"]["id"], repo.id);
        assert_eq!(body["actor"]["login"], "alice");
        assert_eq!(body["pull_requests"], json!([]));
        // Each run has its own check suite, which has the run's state.
        let suite = body["check_suite_id"].as_i64().unwrap();
        let suite_path = format!("/api/v3/repos/alice/project/check-suites/{suite}");
        assert!(body["check_suite_url"]
            .as_str()
            .unwrap()
            .ends_with(&suite_path));
        let (_, check_suite) = checked(
            &api,
            "checks/get-suite",
            "GET",
            &suite_path,
            &reader,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(check_suite["head_sha"], head.sha);
        assert_eq!(check_suite["head_branch"], "main");
        assert_eq!(check_suite["status"], status);
        assert_eq!(check_suite["conclusion"], conclusion);
        let (_, by_suite) = checked(
            &api,
            "actions/list-workflow-runs-for-repo",
            "GET",
            &format!("{base}/runs?check_suite_id={suite}"),
            &reader,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(by_suite["total_count"], 1);
        assert_eq!(by_suite["workflow_runs"][0]["id"], ids[index]);
        assert!(!body.to_string().contains(&repo.disk_path));
        if index == 0 {
            workflow = body["workflow_id"].as_i64().unwrap();
        }
        if index == 2 {
            assert_eq!(body["workflow_id"], workflow);
        }
        let (_, attempt) = checked(
            &api,
            "actions/get-workflow-run-attempt",
            "GET",
            &format!(
                "{base}/runs/{}/attempts/1?exclude_pull_requests=true",
                ids[index]
            ),
            &reader,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(body, attempt);
        checked(
            &api,
            "",
            "GET",
            &format!("{base}/runs/{}/attempts/2", ids[index]),
            &reader,
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    let (headers, body) = checked(
        &api,
        "actions/list-workflow-runs-for-repo",
        "GET",
        &format!("{base}/runs?per_page=1&exclude_pull_requests=true"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_count"], 3);
    assert_eq!(body["workflow_runs"][0]["id"], ids[2]);
    let next = headers["link"].to_str().unwrap();
    assert!(next.contains("/api/v3/repos/alice/project/actions/runs?"));
    assert!(next.contains("exclude_pull_requests=true"));
    assert!(next.contains("page=2"));
    for (query, count) in [
        ("status=completed", 1),
        ("status=failure", 1),
        ("status=in_progress", 1),
        ("status=queued", 1),
        ("status=success", 0),
        ("status=action_required", 0),
        ("actor=alice", 3),
        ("actor=bob", 0),
        ("branch=main", 3),
        ("branch=missing", 0),
        ("event=workflow_dispatch", 3),
        ("event=push", 0),
        ("created=%3E%3D2000-01-01", 3),
        ("created=%3C2000-01-01", 0),
        ("created=2000-01-01..2099-12-31", 3),
        ("check_suite_id=123", 0),
    ] {
        let (_, body) = checked(
            &api,
            "actions/list-workflow-runs-for-repo",
            "GET",
            &format!("{base}/runs?{query}"),
            &reader,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(body["total_count"], count, "{query}");
    }
    let (_, body) = checked(
        &api,
        "actions/list-workflow-runs-for-repo",
        "GET",
        &format!("{base}/runs?head_sha={}", head.sha),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["total_count"], 3);
    for key in ["ci.yml".to_owned(), workflow.to_string()] {
        let (_, body) = checked(
            &api,
            "actions/list-workflow-runs",
            "GET",
            &format!("{base}/workflows/{key}/runs"),
            &reader,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(body["total_count"], 2);
    }
    for query in [
        "status=nonsense",
        "created=not-a-date",
        "created=2099-01-01..2000-01-01",
        "head_sha=not-a-sha",
        "check_suite_id=-1",
    ] {
        checked(
            &api,
            "",
            "GET",
            &format!("{base}/runs?{query}"),
            &reader,
            Value::Null,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    checked(
        &api,
        "",
        "GET",
        &format!("{base}/runs"),
        &outsider,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &api,
        "",
        "GET",
        &format!("{base}/runs/{}", ids[0]),
        &outsider,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    // Native aggregate admission remains available, without inventing a comma-named YAML file.
    let native = codeyard_app::app::AppState::new(
        f.pool().clone(),
        repos.clone(),
        std::sync::Arc::new(
            codeyard_storage::ObjectStore::open(&repos, None)
                .await
                .unwrap(),
        ),
        None,
        codeyard_ci::config::CiConfig {
            trusted_actor_ids: std::collections::BTreeSet::from([alice]),
            ..Default::default()
        },
        None,
    );
    let aggregate = native
        .ci_runs
        .trigger_run(codeyard_repos_core::ci::runs::CiRunRequest {
            repo_id: repo.id,
            repo_owner: "alice".into(),
            repo_name: "project".into(),
            branch: "main".into(),
            head_sha: Some(head.sha.clone()),
            event: "workflow_dispatch".into(),
            pull_request_id: None,
            pipeline: codeyard_ci::config::CiPipelineKind::Ci,
            workflow_paths: vec![
                ".github/workflows/ci.yml".into(),
                ".github/workflows/other.yml".into(),
            ],
            environment: None,
            inputs: None,
            triggered_by_id: alice,
            manual: true,
        })
        .await
        .unwrap();
    assert!(aggregate.workflow_run_number.is_none());
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_workflows WHERE repo_id=$1 AND path LIKE '%,%'",
            &[repo.id]
        )
        .await,
        0
    );
    checked(
        &api,
        "",
        "GET",
        &format!("{base}/runs/{}", aggregate.id),
        &reader,
        Value::Null,
        StatusCode::GONE,
    )
    .await;
    let (_, singles) = checked(
        &api,
        "actions/list-workflow-runs-for-repo",
        "GET",
        &format!("{base}/runs"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(singles["total_count"], 3);
    assert!(singles["workflow_runs"]
        .as_array()
        .unwrap()
        .iter()
        .all(|run| run["id"] != aggregate.id));
    // Workflow identity and history remain usable by gh run after the YAML is removed.
    store
        .save_file_as(
            f.pool(),
            repo.id,
            alice,
            &FileWrite {
                branch: "main".into(),
                path: ".github/workflows/ci.yml".into(),
                message: "Remove workflow".into(),
                expected_revision: head.sha,
                creating: false,
                deleting: true,
            },
            b"",
            "Alice",
            "alice@example.test",
        )
        .await
        .unwrap();
    let (_, historical) = checked(
        &api,
        "actions/get-workflow",
        "GET",
        &format!("{base}/workflows/{workflow}"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(historical["state"], "deleted");
    let (_, history) = checked(
        &api,
        "actions/list-workflow-runs",
        "GET",
        &format!("{base}/workflows/{workflow}/runs"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(history["total_count"], 2);
    assert_eq!(
        codeyard_db_actions::action_workflows::workflow_by_id(f.pool(), repo.id, workflow)
            .await
            .unwrap()
            .unwrap()
            .state,
        "active"
    );
    // Native read authority is rechecked even when the caller kept an earlier repository record.
    f.execute(
        "INSERT INTO repo_access(repo_id,user_id,access_level) VALUES($1,$2,1)",
        &[repo.id, bob],
    )
    .await;
    assert!(codeyard_db_actions::action_runs::list(
        f.pool(),
        Some(bob),
        repo.id,
        &Default::default(),
        0,
        30
    )
    .await
    .unwrap()
    .is_some());
    f.execute(
        "DELETE FROM repo_access WHERE repo_id=$1 AND user_id=$2",
        &[repo.id, bob],
    )
    .await;
    assert!(codeyard_db_actions::action_runs::list(
        f.pool(),
        Some(bob),
        repo.id,
        &Default::default(),
        0,
        30
    )
    .await
    .unwrap()
    .is_none());
    f.finish().await;
}

#[tokio::test]
async fn sqlite_workflow_run_documents() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_workflow_run_documents() {
    contract(true).await;
}
