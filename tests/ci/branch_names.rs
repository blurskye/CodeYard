//! A CI run stores the branch it ran for the way a manual run names it: `main`,
//! not `refs/heads/main`. A pushed tag keeps its full ref, so it cannot be mistaken
//! for a branch. Runs are queued here, not executed.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::fake_webui_bundle as bundle;
use crate::support::native_database as native;
use crate::support::production_ci as production;
use crate::support::pull_requests as requests;
use codeyard::backend::ci::{CiConfig, CiPipelineKind, CiRunRequest, CiRuns};
use codeyard::backend::db;
use std::collections::BTreeSet;

#[tokio::test]
async fn long_account_names_can_list_and_dispatch_workflows_on_both_engines() {
    use axum::http::StatusCode;
    use base64::Engine;
    use codeyard::backend::{
        api::create_router, app::AppState, repositories::lifecycle, storage::ObjectStore,
    };
    use serde_json::{json, Value};
    use std::sync::Arc;

    bundle::install();
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    for pool in [&postgres.pool, &sqlite.pool] {
        db::users::transitions::complete_setup(pool, "root_admin", None, "fixture", "public", None)
            .await
            .unwrap();
        for username in [
            "a".repeat(101),
            "a".repeat(255),
            "é".repeat(51),
            "é".repeat(255),
            "𐐀".repeat(255),
        ] {
            let owner = db::users::accounts::create_user(pool, &username, None)
                .await
                .unwrap();
            let root = tempfile::tempdir().unwrap();
            let repo = lifecycle::provision(
                (pool, None),
                root.path(),
                db::repositories::creation::RepositoryCreationOwner::personal(owner.id),
                "project",
                None,
                false,
                None,
            )
            .await
            .unwrap();
            let router = create_router(AppState::new(
                pool.clone(),
                root.path().to_owned(),
                Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
                None,
                CiConfig {
                    trusted_actor_ids: BTreeSet::from([owner.id]),
                    ..CiConfig::default()
                },
                None,
            ));
            let client = requests::Client::login(&router, pool, owner.id).await;
            let owner_path = codeyard::webui::encode_component(&username);
            let content = base64::engine::general_purpose::STANDARD.encode(
                "name: Manual\non: workflow_dispatch\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo test\n",
            );
            client.expect("POST", &format!("/api/repos/{owner_path}/project/contents/.github/workflows/ci.yml"),
                json!({"content": content, "message": "Workflow", "branch": "main", "expected_revision": "unborn"}), StatusCode::CREATED).await;
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("owner", &username)
                .append_pair("repo", "project")
                .finish();
            let listed = client
                .expect(
                    "GET",
                    &format!("/api/ci/workflows?{query}"),
                    Value::Null,
                    StatusCode::OK,
                )
                .await;
            assert_eq!(listed["workflows"].as_array().unwrap().len(), 1, "{listed}");
            let dispatched = client.expect("POST", "/api/ci/trigger", json!({
                "repo_owner": username, "repo_name": "project", "branch": "main", "event": "workflow_dispatch", "workflow_path": ".github/workflows/ci.yml",
            }), StatusCode::OK).await;
            assert_eq!(dispatched["status"], "queued");
            let reply = client
                .form_reply(
                    &format!("/{owner_path}/project/actions/dispatch"),
                    &[
                        ("branch", "main"),
                        ("workflow_path", ".github/workflows/ci.yml"),
                    ],
                )
                .await;
            assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
            assert_eq!(
                db::ci::list_ci_runs_for_repo(pool, repo.id, 10)
                    .await
                    .unwrap()
                    .len(),
                2
            );
        }
        for (owner, repository) in [
            ("a".repeat(256), "project".to_owned()),
            ("é".repeat(256), "project".to_owned()),
            ("root_admin".to_owned(), "r".repeat(101)),
        ] {
            let root = tempfile::tempdir().unwrap();
            let router = create_router(AppState::new(
                pool.clone(),
                root.path().to_owned(),
                Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
                None,
                CiConfig::default(),
                None,
            ));
            let admin = db::users::accounts::get_user_by_username(pool, "root_admin")
                .await
                .unwrap()
                .unwrap();
            let client = requests::Client::login(&router, pool, admin.id).await;
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("owner", &owner)
                .append_pair("repo", &repository)
                .finish();
            client
                .expect(
                    "GET",
                    &format!("/api/ci/workflows?{query}"),
                    Value::Null,
                    StatusCode::BAD_REQUEST,
                )
                .await;
            client.expect("POST", "/api/ci/trigger", json!({"repo_owner": owner, "repo_name": repository, "branch": "main", "event": "workflow_dispatch"}), StatusCode::BAD_REQUEST).await;
        }
    }
}

#[tokio::test]
async fn manual_runs_take_the_inputs_their_workflow_declares_on_both_engines() {
    use axum::http::StatusCode;
    use base64::Engine;
    use codeyard::backend::{
        api::create_router, app::AppState, repositories::lifecycle, storage::ObjectStore,
    };
    use serde_json::{json, Value};
    use std::sync::Arc;

    bundle::install();
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    let path = ".github/workflows/deploy.yml";
    let workflow = "name: Deploy
on:
  workflow_dispatch:
    inputs:
      target:
        required: true
        type: choice
        options: [staging, production]
      debug:
        type: boolean
        default: false
      retries:
        type: number
        default: 2
jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - run: echo deploy
";
    for pool in [&postgres.pool, &sqlite.pool] {
        db::users::transitions::complete_setup(pool, "root_admin", None, "fixture", "public", None)
            .await
            .unwrap();
        let owner = db::users::accounts::create_user(pool, "inputs_owner", None)
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let repo = lifecycle::provision(
            (pool, None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(owner.id),
            "project",
            None,
            false,
            None,
        )
        .await
        .unwrap();
        let router = create_router(AppState::new(
            pool.clone(),
            root.path().to_owned(),
            Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
            None,
            CiConfig {
                trusted_actor_ids: BTreeSet::from([owner.id]),
                ..CiConfig::default()
            },
            None,
        ));
        let client = requests::Client::login(&router, pool, owner.id).await;
        let content = base64::engine::general_purpose::STANDARD.encode(workflow);
        client
            .expect(
                "POST",
                "/api/repos/inputs_owner/project/contents/.github/workflows/deploy.yml",
                json!({"content": content, "message": "Workflow", "branch": "main", "expected_revision": "unborn"}),
                StatusCode::CREATED,
            )
            .await;
        let dispatch = |inputs: Value| {
            json!({
                "repo_owner": "inputs_owner", "repo_name": "project", "ref": "main",
                "event": "workflow_dispatch", "workflow_path": path, "inputs": inputs,
            })
        };
        // Each refusal names its field and queues nothing.
        for (inputs, reason) in [
            (json!({}), "required"),
            (json!({"target": "staging", "extra": "x"}), "not declared"),
            (json!({"target": "dev"}), "not one of its options"),
            (
                json!({"target": "staging", "debug": "yes"}),
                "true or false",
            ),
            (
                json!({"target": "staging", "retries": "many"}),
                "must be a number",
            ),
            (
                json!({"target": ["staging"]}),
                "strings, numbers or booleans",
            ),
        ] {
            let refused = client
                .expect(
                    "POST",
                    "/api/ci/trigger",
                    dispatch(inputs),
                    StatusCode::BAD_REQUEST,
                )
                .await;
            assert!(
                refused["message"].as_str().unwrap().contains(reason),
                "{refused}"
            );
        }
        let queued = client
            .expect(
                "POST",
                "/api/ci/trigger",
                dispatch(json!({"target": "production", "debug": true, "retries": 3})),
                StatusCode::OK,
            )
            .await;
        assert_eq!(queued["status"], "queued", "{queued}");
        // Inputs left out take their defaults.
        client
            .expect(
                "POST",
                "/api/ci/trigger",
                dispatch(json!({"target": "staging"})),
                StatusCode::OK,
            )
            .await;
        // The form posts each input by name; an unchecked box posts its hidden false.
        let reply = client
            .form_reply(
                "/inputs_owner/project/actions/dispatch",
                &[
                    ("branch", "main"),
                    ("workflow_path", path),
                    ("input:.github/workflows/deploy.yml:target", "production"),
                    ("input:.github/workflows/deploy.yml:debug", "false"),
                    ("input:.github/workflows/deploy.yml:retries", "4"),
                ],
            )
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
        let refused = client
            .form_reply(
                "/inputs_owner/project/actions/dispatch",
                &[
                    ("branch", "main"),
                    ("workflow_path", path),
                    ("input:.github/workflows/deploy.yml:target", "dev"),
                    ("input:.github/workflows/deploy.yml:retries", "4"),
                ],
            )
            .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{}",
            refused.text()
        );
        assert!(refused.text().contains("not one of its options"));
        // The refusal page shows the workflow's fields again, with the values that were typed.
        assert!(refused.text().contains("value=\"4\""), "{}", refused.text());
        // The global form takes the same inputs as name=value lines.
        let global = client
            .form_reply(
                "/ci/dispatch",
                &[
                    ("repo_owner", "inputs_owner"),
                    ("repo_name", "project"),
                    ("branch", "main"),
                    ("event", "workflow_dispatch"),
                    ("workflow_path", path),
                    ("pipeline", "ci"),
                    ("inputs", "target=production\ndebug=true\nretries=4"),
                ],
            )
            .await;
        assert_eq!(global.status, StatusCode::SEE_OTHER, "{}", global.text());
        let malformed = client
            .form_reply(
                "/ci/dispatch",
                &[
                    ("repo_owner", "inputs_owner"),
                    ("repo_name", "project"),
                    ("branch", "main"),
                    ("event", "workflow_dispatch"),
                    ("workflow_path", path),
                    ("pipeline", "ci"),
                    ("inputs", "target production"),
                ],
            )
            .await;
        assert_eq!(
            malformed.status,
            StatusCode::BAD_REQUEST,
            "{}",
            malformed.text()
        );
        assert!(
            malformed.text().contains("name=value"),
            "{}",
            malformed.text()
        );
        assert!(
            malformed.text().contains("target production"),
            "{}",
            malformed.text()
        );
        let stored: Vec<Value> = db::ci::list_ci_runs_for_repo(pool, repo.id, 20)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|run| run.workflow_inputs)
            .collect();
        assert_eq!(stored.len(), 4, "{stored:?}");
        assert!(stored.contains(&json!({"target": "production", "debug": "true", "retries": "4"})));
        assert!(stored.contains(&json!({"target": "production", "debug": "true", "retries": "3"})));
        assert!(stored.contains(&json!({"target": "staging", "debug": "false", "retries": "2"})));
        assert!(stored.contains(&json!({"target": "production", "debug": "false", "retries": "4"})));
    }
}

#[test]
fn only_branches_lose_their_ref_prefix() {
    use codeyard::backend::ci::branch_name;
    assert_eq!(branch_name("refs/heads/main"), "main");
    assert_eq!(branch_name("refs/heads/feature/x"), "feature/x");
    assert_eq!(branch_name("main"), "main");
    assert_eq!(branch_name("refs/tags/v1"), "refs/tags/v1");
}

#[tokio::test]
async fn queued_runs_use_short_branch_names_and_tags_stay_distinct() {
    let mut f = production::Fixture::new().await;
    let git = git2::Repository::open_bare(f.temp.path().join("repo.git")).unwrap();
    let tip = git2::Oid::from_str(&f.sha).unwrap();
    git.tag_lightweight("v1", &git.find_object(tip, None).unwrap(), false)
        .unwrap();
    drop(git);
    let service = CiRuns::new(
        f.pool.clone(),
        std::sync::Arc::new(CiConfig {
            trusted_actor_ids: BTreeSet::from([f.owner.id]),
            ..CiConfig::default()
        }),
        std::sync::Arc::new(tokio::sync::Notify::new()),
        None,
        std::sync::Arc::new(
            codeyard::backend::notifications::NotificationBroadcaster::new(&f.pool),
        ),
    );
    let request = |branch: &str, event: &str| CiRunRequest {
        repo_id: f.repo.id,
        repo_owner: f.owner.username.clone(),
        repo_name: f.repo.name.clone(),
        branch: branch.to_owned(),
        head_sha: Some(f.sha.clone()),
        event: event.to_owned(),
        pull_request_id: None,
        pipeline: CiPipelineKind::Ci,
        workflow_paths: vec![".github/workflows/ci.yml".to_owned()],
        environment: None,
        inputs: None,
        triggered_by_id: f.owner.id,
    };
    // A push arrives as the full ref.
    let pushed = service
        .trigger_run(request("refs/heads/main", "push"))
        .await
        .unwrap();
    assert_eq!(pushed.branch, "main");
    // A manual run is given the short name.
    let manual = service
        .trigger_run(request("main", "workflow_dispatch"))
        .await
        .unwrap();
    assert_eq!(manual.branch, "main");
    // A tag keeps its ref.
    let tagged = service
        .trigger_run(request("refs/tags/v1", "push"))
        .await
        .unwrap();
    assert_eq!(tagged.branch, "refs/tags/v1");
    // A manual run given the short name of a tag is stored as that tag too.
    let manual_tag = service
        .trigger_run(request("v1", "workflow_dispatch"))
        .await
        .unwrap();
    assert_eq!(manual_tag.branch, "refs/tags/v1");
    // JSON and native form dispatch enter this same HTTP-free operation.
    let dispatched = service
        .dispatch_run(
            f.owner.id,
            codeyard::backend::ci::runs::DispatchRequest {
                repo_owner: f.owner.username.clone(),
                repo_name: f.repo.name.clone(),
                branch: "refs/heads/main".to_owned(),
                event: "workflow_dispatch".to_owned(),
                head_sha: Some(f.sha.clone()),
                workflow_path: Some(".github/workflows/ci.yml".to_owned()),
                pipeline: Some("ci".to_owned()),
                environment: None,
                inputs: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(dispatched.branch, manual.branch);
    assert_eq!(dispatched.head_sha, manual.head_sha);
    assert_eq!(dispatched.workflow_blobs, manual.workflow_blobs);
    assert_eq!(dispatched.status, "queued");
    let refused = service
        .dispatch_run(
            f.owner.id,
            codeyard::backend::ci::runs::DispatchRequest {
                repo_owner: f.owner.username.clone(),
                repo_name: f.repo.name.clone(),
                branch: "../private".to_owned(),
                event: "workflow_dispatch".to_owned(),
                head_sha: None,
                workflow_path: None,
                pipeline: None,
                environment: None,
                inputs: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        refused,
        codeyard::backend::ci::admission::AdmissionError::InvalidRequest(_)
    ));
    // What the database holds is what the run list shows.
    let stored: Vec<String> =
        sqlx::query_scalar("SELECT branch FROM ci_runs WHERE repo_id = $1 ORDER BY id")
            .bind(f.repo.id)
            .fetch_all(&f.raw)
            .await
            .unwrap();
    assert_eq!(
        stored,
        ["main", "main", "refs/tags/v1", "refs/tags/v1", "main"]
    );
    // A push is recorded under its full ref. After a force-push moves the branch, a rerun of
    // that push still runs the commit it pushed.
    db::ci::record_ci_accepted_push(&f.pool, f.repo.id, f.owner.id, "refs/heads/main", &f.sha)
        .await
        .unwrap();
    let git = git2::Repository::open_bare(f.temp.path().join("repo.git")).unwrap();
    production::commit(&git, "refs/heads/main", None, "force-pushed", None);
    drop(git);
    let rerun = service
        .dispatch_run(
            f.owner.id,
            codeyard::backend::ci::runs::DispatchRequest {
                repo_owner: f.owner.username.clone(),
                repo_name: f.repo.name.clone(),
                branch: "main".to_owned(),
                event: "push".to_owned(),
                head_sha: Some(f.sha.clone()),
                workflow_path: Some(".github/workflows/ci.yml".to_owned()),
                pipeline: Some("ci".to_owned()),
                environment: None,
                inputs: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(rerun.branch, "main");
    assert_eq!(rerun.head_sha.as_deref(), Some(f.sha.as_str()));
    f.finish().await;
}
