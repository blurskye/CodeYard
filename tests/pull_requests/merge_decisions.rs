//! Protected merges require every independent CI producer, on SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;

use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::{CiConfig, CiPipelineKind},
    db,
    storage::ObjectStore,
};
use database::Database;
use pulls::{text, Client};
use serde_json::json;
use std::{collections::HashMap, sync::Arc};

fn rule<'a>(
    repo: i64,
    actor: i64,
    pattern: &'a str,
) -> db::repositories::protection::BranchProtectionRuleUpsert<'a> {
    db::repositories::protection::BranchProtectionRuleUpsert {
        actor_id: actor,
        repo_id: repo,
        pattern,
        require_pull_request: true,
        required_approving_reviews: 0,
        dismiss_stale_reviews: false,
        require_status_checks: false,
        required_status_checks: vec![],
        ..Default::default()
    }
}

async fn enqueue(
    pool: &db::DbPool,
    repo: i64,
    actor: i64,
    sha: &str,
    pipeline: &str,
    path: &str,
) -> db::models::CiRun {
    db::ci::enqueue_ci_run(
        pool,
        db::ci::NewCiRun {
            repo_id: repo,
            triggered_by_id: actor,
            pipeline,
            event: "manual",
            pull_request_id: None,
            branch: "main",
            head_sha: sha,
            workflow_path: path,
            workflow_blobs: &json!({}),
            workflow_inputs: None,
            environment: None,
            manual: false,
        },
        &[actor],
    )
    .await
    .unwrap()
}

async fn blocked(client: &Client, number: i64, expected: &str) {
    let pull = client.pull(number).await;
    let error = client
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/merge"),
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"]}),
            StatusCode::CONFLICT,
        )
        .await;
    assert_eq!(error["error"], "branch_protection", "{error}");
    assert!(text(&error, "message").contains(expected), "{error}");
    assert_eq!(client.pull(number).await["state"], "open");
}

async fn contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("alice").await;
    let merger = f.actor("late_merger").await;
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        merger,
        repository.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let state = Arc::new(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let router = create_router((*state).clone());
    let alice = Client::login(&router, f.pool(), owner).await;
    let mut base = alice.save("main", "README.md", "unborn").await;

    alice.branch("late-policy", &base).await;
    let late_head = alice.save("late-policy", "late.txt", &base).await;
    let late_pull = alice.open_pull("Late policy", "late-policy", "main").await;
    let late_number = late_pull["number"].as_i64().unwrap() as i32;
    let prepared =
        codeyard::backend::repositories::pull_requests::merge::prepare_pull_request_merge(
            f.pool(),
            &state.ci_runs,
            merger,
            ("alice".into(), "proj".into(), late_number),
            codeyard::backend::repositories::pull_requests::merge::MergeRequest {
                expected_head: late_head.clone(),
                expected_base: base.clone(),
                expected_base_branch: None,
                expected_target: None,
                merge_method: Default::default(),
            },
        )
        .await
        .unwrap();
    let mut late_rule = rule(repository.id, owner, "main");
    late_rule.required_approving_reviews = 1;
    db::repositories::protection::upsert_branch_protection_rule(f.pool(), late_rule)
        .await
        .unwrap();
    assert!(matches!(
        codeyard::backend::repositories::pull_requests::merge::publish_prepared_merge(
            f.pool(),
            &state.ci_runs,
            &state.notification_broadcaster,
            prepared,
        )
        .await,
        Err(
            codeyard::backend::repositories::pull_requests::OperationError::Conflict {
                code: "branch_protection",
                ..
            }
        )
    ));
    assert_eq!(
        git.find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        base
    );
    assert_eq!(alice.pull(late_number as i64).await["state"], "open");
    assert!(db::repositories::protection::delete_branch_protection_rule(
        f.pool(),
        owner,
        repository.id,
        "main",
    )
    .await
    .unwrap());
    let prepared =
        codeyard::backend::repositories::pull_requests::merge::prepare_pull_request_merge(
            f.pool(),
            &state.ci_runs,
            merger,
            ("alice".into(), "proj".into(), late_number),
            codeyard::backend::repositories::pull_requests::merge::MergeRequest {
                expected_head: late_head.clone(),
                expected_base: base.clone(),
                expected_base_branch: None,
                expected_target: None,
                merge_method: Default::default(),
            },
        )
        .await
        .unwrap();
    f.execute(
        "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
        &[repository.id],
    )
    .await;
    assert!(matches!(
        codeyard::backend::repositories::pull_requests::merge::publish_prepared_merge(
            f.pool(),
            &state.ci_runs,
            &state.notification_broadcaster,
            prepared,
        )
        .await,
        Err(codeyard::backend::repositories::pull_requests::OperationError::Denied { .. })
    ));
    f.execute(
        "UPDATE repositories SET is_archived=FALSE WHERE id=$1",
        &[repository.id],
    )
    .await;
    assert_eq!(alice.pull(late_number as i64).await["state"], "open");
    assert_eq!(
        git.find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        base
    );
    assert_eq!(
        db::notifications::records::count_unread_notifications(f.pool(), owner)
            .await
            .unwrap(),
        0
    );
    assert!(db::ci::latest_ci_runs_for_commit(
        f.pool(),
        repository.id,
        &late_head,
        late_pull["id"].as_i64().unwrap(),
        "main",
    )
    .await
    .unwrap()
    .is_empty());

    // Commit ids have one lowercase identity. A differently-cased update replaces
    // the same context.
    alice.branch("case-status", &base).await;
    let case_head = alice.save("case-status", "case.txt", &base).await;
    let case_upper = case_head.to_ascii_uppercase();
    assert_ne!(case_upper, case_head);
    let case_pull = alice
        .open_pull("Case-insensitive statuses", "case-status", "main")
        .await;
    let case_number = case_pull["number"].as_i64().unwrap();
    let mut case_rule = rule(repository.id, owner, "main");
    case_rule.require_status_checks = true;
    case_rule.required_status_checks = vec!["external/check".into()];
    db::repositories::protection::upsert_branch_protection_rule(f.pool(), case_rule)
        .await
        .unwrap();
    alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/commits/{case_head}/statuses"),
            json!({"context": "external/check", "state": "success"}),
            StatusCode::CREATED,
        )
        .await;
    let failed = alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/commits/{case_upper}/statuses"),
            json!({"context": "external/check", "state": "failure"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(failed["sha"], case_head);
    let statuses =
        db::repositories::protection::list_commit_statuses(f.pool(), repository.id, &case_upper)
            .await
            .unwrap();
    assert_eq!(statuses.len(), 1);
    assert!(statuses
        .iter()
        .all(|status| status.sha == case_head && status.state == "failure"));
    blocked(&alice, case_number, "external/check (failure)").await;
    assert!(db::repositories::protection::delete_branch_protection_rule(
        f.pool(),
        owner,
        repository.id,
        "main",
    )
    .await
    .unwrap());

    // Unsupported patterns are rejected at both write boundaries without changing rules.
    db::repositories::protection::upsert_branch_protection_rule(
        f.pool(),
        rule(repository.id, owner, "release/*"),
    )
    .await
    .unwrap();
    for pattern in ["", "!release", "release/[ab]", "release\\*", "bad..name"] {
        let error = db::repositories::protection::upsert_branch_protection_rule(
            f.pool(),
            rule(repository.id, owner, pattern),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<db::repositories::protection::ProtectionWriteRefusal>(),
            Some(&db::repositories::protection::ProtectionWriteRefusal::InvalidPattern)
        );
        let error = alice
            .expect(
                "PUT",
                "/api/repos/alice/proj/branch-protection",
                json!({"pattern": pattern}),
                StatusCode::BAD_REQUEST,
            )
            .await;
        assert_eq!(error["error"], "validation_error");
        let rules =
            db::repositories::protection::list_branch_protection_rules(f.pool(), repository.id)
                .await
                .unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].pattern, "release/*");
    }
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/branches",
            json!({"name": "release/1", "sha": base}),
            StatusCode::FORBIDDEN,
        )
        .await;
    assert!(git.find_reference("refs/heads/release/1").is_err());

    let grouped = CiConfig::default()
        .workflow_paths(CiPipelineKind::Full)
        .join(",");
    let long_name = "a".repeat(240);
    let pairs = [
        (
            "ci",
            ".github/workflows/test.yml".to_owned(),
            "ci",
            ".github/workflows/test.yaml".to_owned(),
            "gitserv/test".to_owned(),
        ),
        (
            "ci",
            ".github/workflows/full.yml".to_owned(),
            "full",
            grouped,
            "gitserv/full".to_owned(),
        ),
        (
            "ci",
            format!(".github/workflows/{long_name}.yml"),
            "ci",
            format!(".github/workflows/{long_name}.yaml"),
            format!("gitserv/{long_name}"),
        ),
    ];
    for (pair, (pipeline_a, path_a, pipeline_b, path_b, alias)) in pairs.iter().enumerate() {
        for success_first in [false, true] {
            let branch = format!("topic-{pair}-{success_first}");
            alice.branch(&branch, &base).await;
            let head = alice.save(&branch, &format!("{branch}.txt"), &base).await;
            let pull = alice.open_pull("Checks", &branch, "main").await;
            let number = pull["number"].as_i64().unwrap();
            alice
                .expect(
                    "PUT",
                    "/api/repos/alice/proj/branch-protection",
                    json!({"pattern": "main", "require_pull_request": true,
                "require_status_checks": true, "required_status_checks": [alias]}),
                    StatusCode::OK,
                )
                .await;
            let a = enqueue(f.pool(), repository.id, owner, &head, pipeline_a, path_a).await;
            let b = enqueue(f.pool(), repository.id, owner, &head, pipeline_b, path_b).await;
            let context_a = a.commit_status_context();
            let context_b = b.commit_status_context();
            assert_ne!(context_a, context_b);
            assert_eq!(a.alias_commit_status_context(), *alias);
            assert_eq!(b.alias_commit_status_context(), *alias);
            if pair == 2 {
                assert!(context_a.len() > 255);
            }

            blocked(&alice, number, "pending").await;
            for context in [alias, &context_a, &context_b] {
                let error = db::repositories::protection::upsert_commit_status(
                    f.pool(),
                    db::repositories::protection::CommitStatusUpsert {
                        repo_id: repository.id,
                        sha: &head,
                        context,
                        state: "success",
                        description: None,
                        target_url: None,
                        creator_id: Some(owner),
                    },
                )
                .await
                .unwrap_err();
                assert_eq!(error.to_string(), "server-owned commit status context");
                let error = alice
                    .expect(
                        "POST",
                        &format!("/api/repos/alice/proj/commits/{head}/statuses"),
                        json!({"context": context, "state": "success"}),
                        StatusCode::FORBIDDEN,
                    )
                    .await;
                assert_eq!(error["error"], "reserved_context");
            }
            // Each run is taken by its own slot, since a slot holds one run at a time.
            for expected in [&a, &b] {
                let claimed = db::ci::claim_ci_run(
                    f.pool(),
                    &format!("worker-{}", expected.id),
                    "local",
                    &[owner],
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(claimed.id, expected.id);
            }
            let order = if success_first {
                [(&b, "success"), (&a, "failed")]
            } else {
                [(&a, "failed"), (&b, "success")]
            };
            for (index, (run, status)) in order.into_iter().enumerate() {
                assert!(db::ci::finish_ci_run(
                    f.pool(),
                    run.id,
                    &format!("worker-{}", run.id),
                    status,
                    None
                )
                .await
                .unwrap()
                .is_some());
                blocked(
                    &alice,
                    number,
                    if index == 0 && success_first {
                        "pending"
                    } else {
                        "failure"
                    },
                )
                .await;
            }
            let statuses =
                db::repositories::protection::list_commit_statuses(f.pool(), repository.id, &head)
                    .await
                    .unwrap();
            let states: HashMap<_, _> = statuses
                .iter()
                .map(|s| (s.context.as_str(), s.state.as_str()))
                .collect();
            assert_eq!(states.get(context_a.as_str()), Some(&"failure"));
            assert_eq!(states.get(context_b.as_str()), Some(&"success"));
            let latest =
                db::ci::latest_ci_runs_for_commit(f.pool(), repository.id, &head, 0, "main")
                    .await
                    .unwrap();
            assert_eq!(latest.len(), 2);

            // Retries replace only this source. A late older failure cannot replace success.
            let old_retry =
                enqueue(f.pool(), repository.id, owner, &head, pipeline_a, path_a).await;
            blocked(&alice, number, "pending").await;
            assert_eq!(
                db::ci::claim_ci_run(f.pool(), "worker", "local", &[owner])
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
                old_retry.id
            );
            let retry = enqueue(f.pool(), repository.id, owner, &head, pipeline_a, path_a).await;
            blocked(&alice, number, "pending").await;
            // Another slot takes the retry, since the first one still holds its run.
            assert_eq!(
                db::ci::claim_ci_run(f.pool(), "worker-2", "local", &[owner])
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
                retry.id
            );
            db::ci::finish_ci_run(f.pool(), retry.id, "worker-2", "success", None)
                .await
                .unwrap()
                .unwrap();
            db::ci::finish_ci_run(f.pool(), old_retry.id, "worker", "failed", None)
                .await
                .unwrap()
                .unwrap();
            let statuses =
                db::repositories::protection::list_commit_statuses(f.pool(), repository.id, &head)
                    .await
                    .unwrap();
            assert_eq!(
                statuses
                    .iter()
                    .find(|s| s.context == context_a)
                    .unwrap()
                    .state,
                "success"
            );
            let latest =
                db::ci::latest_ci_runs_for_commit(f.pool(), repository.id, &head, 0, "main")
                    .await
                    .unwrap();
            assert_eq!(latest.len(), 2);
            assert!(latest.iter().all(|run| run.status == "success"));
            assert!(latest.iter().any(|run| run.id == retry.id));

            // Unknown server-owned requirements remain missing even with a stored success.
            alice
                .expect(
                    "PUT",
                    "/api/repos/alice/proj/branch-protection",
                    json!({"pattern": "main", "require_status_checks": true,
                "required_status_checks": ["gitserv/unknown"]}),
                    StatusCode::OK,
                )
                .await;
            f.execute(&format!("INSERT INTO commit_statuses(repo_id,sha,context,state) VALUES($1,'{head}','gitserv/unknown','success')"), &[repository.id]).await;
            blocked(&alice, number, "gitserv/unknown (missing)").await;
            alice.expect("PUT", "/api/repos/alice/proj/branch-protection", json!({"pattern": "main", "require_pull_request": true,
                "require_status_checks": true, "required_status_checks": [alias, &context_a, &context_b]}), StatusCode::OK).await;
            assert_eq!(
                git.find_reference("refs/heads/main")
                    .unwrap()
                    .target()
                    .unwrap()
                    .to_string(),
                base
            );
            let merged = alice.merge_current(number).await;
            base = text(&merged, "sha").to_owned();
            assert_eq!(
                git.find_reference("refs/heads/main")
                    .unwrap()
                    .target()
                    .unwrap()
                    .to_string(),
                base
            );
        }
    }
    drop(git);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_protected_merges_require_independent_ci_sources() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_protected_merges_require_independent_ci_sources() {
    contract(true).await;
}

/// Merging a pull request with `client`, answering with the status and the error or pull.
async fn merge_as(client: &Client, number: i64) -> (StatusCode, serde_json::Value) {
    let pull = client.pull(number).await;
    client
        .api(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/merge"),
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"]}),
        )
        .await
}

async fn protection_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("alice").await;
    let writer = f.actor("bob").await;
    let reader = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    for (user, level) in [
        (writer, db::models::AccessLevel::Write),
        (reader, db::models::AccessLevel::Read),
    ] {
        db::repositories::access::grant_repo_access(f.pool(), user, repository.id, level)
            .await
            .unwrap();
    }
    let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let state = Arc::new(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let router = create_router((*state).clone());
    let alice = Client::login(&router, f.pool(), owner).await;
    let bob = Client::login(&router, f.pool(), writer).await;
    let carol = Client::login(&router, f.pool(), reader).await;
    let base = alice.save("main", "README.md", "unborn").await;

    // A request that names only some fields gets the strict defaults for the others.
    let saved = alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({"pattern": "main", "require_conversation_resolution": true}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(saved["require_pull_request"], true, "{saved}");
    assert_eq!(saved["dismiss_stale_reviews"], true, "{saved}");
    assert_eq!(saved["enforce_admins"], true, "{saved}");
    assert_eq!(saved["allow_force_pushes"], false, "{saved}");
    assert_eq!(saved["allow_deletions"], false, "{saved}");
    assert_eq!(saved["require_conversation_resolution"], true, "{saved}");
    assert_eq!(saved["push_allowances"], json!([]), "{saved}");

    // Direct writes stay refused for everyone, the owner included, with the reason.
    for client in [&alice, &bob] {
        let refused = client
            .expect(
                "POST",
                "/api/repos/alice/proj/contents/direct.txt",
                json!({"content": "ZGlyZWN0Cg==", "message": "Direct", "branch": "main", "expected_revision": base}),
                StatusCode::FORBIDDEN,
            )
            .await;
        assert!(
            text(&refused, "message").contains("pull request"),
            "{refused}"
        );
    }
    alice
        .expect(
            "DELETE",
            "/api/repos/alice/proj/branches/main",
            json!({}),
            StatusCode::FORBIDDEN,
        )
        .await;

    // One unresolved conversation blocks the merge until someone resolves it.
    alice.branch("feature", &base).await;
    alice.save("feature", "a.txt", &base).await;
    let pull = alice.open_pull("Feature", "feature", "main").await;
    let number = pull["number"].as_i64().unwrap();
    let comments = format!("/api/repos/alice/proj/pulls/{number}/comments");
    let thread = bob
        .expect(
            "POST",
            &comments,
            json!({"body": "Please change this", "path": "a.txt", "line": 1, "side": "RIGHT"}),
            StatusCode::CREATED,
        )
        .await;
    assert!(thread["resolved_at"].is_null(), "{thread}");
    let thread_id = thread["id"].as_i64().unwrap();
    let reply = alice
        .expect(
            "POST",
            &comments,
            json!({"body": "Done", "in_reply_to_comment_id": thread_id}),
            StatusCode::CREATED,
        )
        .await;
    let (status, error) = merge_as(&alice, number).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"], "unresolved_conversations", "{error}");
    assert!(text(&error, "message").contains('1'), "{error}");
    assert_eq!(alice.pull(number).await["state"], "open");

    let resolve = format!("{comments}/{thread_id}/resolve");
    let reply_id = reply["id"].as_i64().unwrap();
    carol
        .expect("POST", &resolve, json!({}), StatusCode::FORBIDDEN)
        .await;
    alice
        .expect(
            "POST",
            &format!("{comments}/{reply_id}/resolve"),
            json!({}),
            StatusCode::BAD_REQUEST,
        )
        .await;
    let resolved = bob
        .expect("POST", &resolve, json!({}), StatusCode::OK)
        .await;
    assert!(!resolved["resolved_at"].is_null(), "{resolved}");
    assert_eq!(resolved["resolved_by_id"], writer, "{resolved}");
    let listed = alice
        .expect("GET", &comments, json!({}), StatusCode::OK)
        .await;
    let listed = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|comment| comment["id"] == thread_id)
        .unwrap();
    assert_eq!(listed["resolved_by_id"], writer, "{listed}");
    let reopened = alice
        .expect(
            "POST",
            &format!("{comments}/{thread_id}/unresolve"),
            json!({}),
            StatusCode::OK,
        )
        .await;
    assert!(reopened["resolved_at"].is_null(), "{reopened}");
    assert!(reopened["resolved_by_id"].is_null(), "{reopened}");
    let (status, error) = merge_as(&alice, number).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    alice
        .expect("POST", &resolve, json!({}), StatusCode::OK)
        .await;
    let (status, merged) = merge_as(&alice, number).await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert_eq!(alice.pull(number).await["state"], "merged");

    // An administrator skips the rule once enforce_admins is off. A writer does not.
    let base = git
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap()
        .to_string();
    alice.branch("second", &base).await;
    alice.save("second", "b.txt", &base).await;
    let second = alice.open_pull("Second", "second", "main").await["number"]
        .as_i64()
        .unwrap();
    bob.expect(
        "POST",
        &format!("/api/repos/alice/proj/pulls/{second}/comments"),
        json!({"body": "Open question", "path": "b.txt", "line": 1, "side": "RIGHT"}),
        StatusCode::CREATED,
    )
    .await;
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({"pattern": "main", "require_conversation_resolution": true, "enforce_admins": false}),
            StatusCode::OK,
        )
        .await;
    let (status, error) = merge_as(&bob, second).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"], "unresolved_conversations", "{error}");
    let (status, merged) = merge_as(&alice, second).await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    let tip = git
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap()
        .to_string();
    let direct = json!({"content": "ZGlyZWN0Cg==", "message": "Direct", "branch": "main", "expected_revision": tip});
    bob.expect(
        "POST",
        "/api/repos/alice/proj/contents/direct.txt",
        direct.clone(),
        StatusCode::FORBIDDEN,
    )
    .await;
    let saved = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/contents/direct.txt",
            direct,
            StatusCode::CREATED,
        )
        .await;
    assert!(saved["sha"].is_string(), "{saved}");
    // The editor save is a branch update in the owner's activity, next to pushes.
    let activity = db::activity::list_user_activity(f.pool(), owner, Some(owner), 50, 0)
        .await
        .unwrap();
    assert!(
        activity.rows.iter().any(|row| row.kind == "push"
            && row.branch.as_deref() == Some("main")
            && row.commit_count == Some(1)),
        "a saved commit is recorded as a push of one commit"
    );

    // Fields the forge cannot enforce are refused when they are written.
    for field in ["require_signed_commits"] {
        let error = alice
            .expect(
                "PUT",
                "/api/repos/alice/proj/branch-protection",
                json!({"pattern": "main", field: true}),
                StatusCode::BAD_REQUEST,
            )
            .await;
        assert_eq!(error["error"], "unsupported_protection", "{error}");
    }
    let error = alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({"pattern": "main", "restrict_pushes": true, "push_allowances": ["carol"]}),
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert_eq!(error["error"], "validation_error", "{error}");

    // A list is refused past 100 entries, repeats included; a list of one writer is stored once.
    for (count, status) in [
        (100, StatusCode::OK),
        (101, StatusCode::BAD_REQUEST),
        (5000, StatusCode::BAD_REQUEST),
    ] {
        let (code, body) = alice
            .api(
                "PUT",
                "/api/repos/alice/proj/branch-protection",
                json!({"pattern": "release/**", "restrict_pushes": true, "push_allowances": vec!["bob"; count]}),
            )
            .await;
        assert_eq!(code, status, "{body}");
        if status == StatusCode::OK {
            assert_eq!(body["push_allowances"], json!(["bob"]), "{body}");
        } else {
            assert_eq!(body["error"], "validation_error", "{body}");
        }
    }

    // Glob patterns are saved, listed and deleted by their encoded form.
    let saved = alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({"pattern": "release/**", "restrict_pushes": true, "push_allowances": ["bob"]}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(saved["push_allowances"], json!(["bob"]), "{saved}");
    alice
        .expect(
            "DELETE",
            "/api/repos/alice/proj/branch-protection/release%2F**",
            json!({}),
            StatusCode::NO_CONTENT,
        )
        .await;
    let rules = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/branch-protection",
            json!({}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(rules.as_array().unwrap().len(), 1, "{rules}");
    drop(git);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_branch_protection_rules_gate_writes_and_merges() {
    protection_contract(false).await;
}

#[tokio::test]
async fn postgres_branch_protection_rules_gate_writes_and_merges() {
    protection_contract(true).await;
}

/// The merge method cannot land on a branch that requires a linear history, even though the head
/// is up to date. Without that rule the same merge lands, as a merge commit of base and head.
async fn linear_history_merge_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let state = Arc::new(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let router = create_router((*state).clone());
    let alice = Client::login(&router, f.pool(), owner).await;
    let base = alice.save("main", "README.md", "unborn").await;

    alice.branch("feature", &base).await;
    let feature = alice.save("feature", "feature.txt", &base).await;
    alice.branch("side", &base).await;
    let side = alice.save("side", "side.txt", &base).await;
    // The feature head becomes a merge of `side` above the base. Its tree is the feature
    // tip's, which is enough for the history check.
    let merged = {
        let signature = git2::Signature::now("alice", "alice@example.test").unwrap();
        let ours = git
            .find_commit(git2::Oid::from_str(&feature).unwrap())
            .unwrap();
        let theirs = git
            .find_commit(git2::Oid::from_str(&side).unwrap())
            .unwrap();
        let tree = ours.tree().unwrap();
        let oid = git
            .commit(
                None,
                &signature,
                &signature,
                "Merge side",
                &tree,
                &[&ours, &theirs],
            )
            .unwrap();
        git.reference("refs/heads/feature", oid, true, "merge side")
            .unwrap();
        oid.to_string()
    };
    let pull = alice.open_pull("Merge side", "feature", "main").await;
    let number = pull["number"].as_i64().unwrap();
    assert_eq!(pull["head_sha"], merged.as_str());

    let mut linear = rule(repository.id, owner, "main");
    linear.require_linear_history = true;
    db::repositories::protection::upsert_branch_protection_rule(f.pool(), linear)
        .await
        .unwrap();
    let (status, error) = merge_as(&alice, number).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"], "branch_protection", "{error}");
    assert!(
        text(&error, "message").contains("Merge commits are not allowed"),
        "{error}"
    );
    assert_eq!(alice.pull(number).await["state"], "open");
    assert_eq!(
        git.refname_to_id("refs/heads/main").unwrap().to_string(),
        base
    );

    let mut plain = rule(repository.id, owner, "main");
    plain.require_linear_history = false;
    db::repositories::protection::upsert_branch_protection_rule(f.pool(), plain)
        .await
        .unwrap();
    let (status, error) = merge_as(&alice, number).await;
    assert_eq!(status, StatusCode::OK, "{error}");
    assert_eq!(alice.pull(number).await["state"], "merged");
    let landed = git.refname_to_id("refs/heads/main").unwrap();
    assert_eq!(
        git.find_commit(landed)
            .unwrap()
            .parent_ids()
            .collect::<Vec<_>>(),
        vec![
            git2::Oid::from_str(&base).unwrap(),
            git2::Oid::from_str(&merged).unwrap()
        ]
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_linear_history_refuses_merge_commits_from_a_pull_request() {
    linear_history_merge_contract(false).await;
}

#[tokio::test]
async fn postgres_linear_history_refuses_merge_commits_from_a_pull_request() {
    linear_history_merge_contract(true).await;
}

/// A merge commit that no ref reaches cannot become a new branch on a branch that requires a
/// linear history. The branch API reads the history of the commit it points the branch at.
async fn linear_history_branch_creation_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let state = Arc::new(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let router = create_router((*state).clone());
    let alice = Client::login(&router, f.pool(), owner).await;
    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("side", &base).await;
    let side = alice.save("side", "side.txt", &base).await;
    // A merge of `side` into `base` that no ref points at. This is the state a deleted scratch
    // branch leaves behind before garbage collection.
    let merged = {
        let signature = git2::Signature::now("alice", "alice@example.test").unwrap();
        let ours = git
            .find_commit(git2::Oid::from_str(&base).unwrap())
            .unwrap();
        let theirs = git
            .find_commit(git2::Oid::from_str(&side).unwrap())
            .unwrap();
        let tree = theirs.tree().unwrap();
        git.commit(
            None,
            &signature,
            &signature,
            "Merge side",
            &tree,
            &[&ours, &theirs],
        )
        .unwrap()
        .to_string()
    };
    let mut linear = rule(repository.id, owner, "release/**");
    linear.require_pull_request = false;
    linear.require_linear_history = true;
    db::repositories::protection::upsert_branch_protection_rule(f.pool(), linear)
        .await
        .unwrap();

    let refused = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/branches",
            json!({"name": "release/v1", "sha": merged}),
            StatusCode::FORBIDDEN,
        )
        .await;
    assert_eq!(refused["error"], "protected_branch", "{refused}");
    assert!(
        text(&refused, "message").contains("Merge commits are not allowed"),
        "{refused}"
    );
    assert!(git.find_reference("refs/heads/release/v1").is_err());

    // The same protected branch can still start from an ordinary commit.
    alice.branch("release/v2", &side).await;
    assert_eq!(
        git.refname_to_id("refs/heads/release/v2")
            .unwrap()
            .to_string(),
        side
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_linear_history_refuses_a_merge_commit_as_a_new_branch() {
    linear_history_branch_creation_contract(false).await;
}

#[tokio::test]
async fn postgres_linear_history_refuses_a_merge_commit_as_a_new_branch() {
    linear_history_branch_creation_contract(true).await;
}

/// A queued `pull_request` run of `pull`, which targets `base`.
async fn enqueue_pull_request_run(
    pool: &db::DbPool,
    repo: i64,
    actor: i64,
    pull: i64,
    base: &str,
    sha: &str,
) -> db::models::CiRun {
    db::ci::enqueue_ci_run(
        pool,
        db::ci::NewCiRun {
            repo_id: repo,
            triggered_by_id: actor,
            pipeline: "ci",
            event: "pull_request",
            pull_request_id: Some(pull),
            branch: base,
            head_sha: sha,
            workflow_path: ".github/workflows/test.yml",
            workflow_blobs: &json!({}),
            workflow_inputs: None,
            environment: None,
            manual: false,
        },
        &[actor],
    )
    .await
    .unwrap()
}

/// Two pull requests can share a head commit when their base branches differ. A run of one
/// must not satisfy the other's required check, and a newer run of the other must not hide the
/// failure.
async fn runs_of_another_pull_request_do_not_satisfy_a_check(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let state = Arc::new(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let router = create_router((*state).clone());
    let alice = Client::login(&router, f.pool(), owner).await;
    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("release", &base).await;
    alice.branch("topic", &base).await;
    let head = alice.save("topic", "topic.txt", &base).await;
    let to_main = alice.open_pull("To main", "topic", "main").await;
    let to_release = alice.open_pull("To release", "topic", "release").await;
    let main_number = to_main["number"].as_i64().unwrap();
    let release_number = to_release["number"].as_i64().unwrap();
    for branch in ["main", "release"] {
        alice
            .expect(
                "PUT",
                "/api/repos/alice/proj/branch-protection",
                json!({"pattern": branch, "require_pull_request": true,
                    "require_status_checks": true,
                    "required_status_checks": ["gitserv/workflow/.github/workflows/test.yml"]}),
                StatusCode::OK,
            )
            .await;
    }

    let failed = enqueue_pull_request_run(
        f.pool(),
        repository.id,
        owner,
        to_main["id"].as_i64().unwrap(),
        "main",
        &head,
    )
    .await;
    let claimed = db::ci::claim_ci_run(f.pool(), "worker", "local", &[owner])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, failed.id);
    db::ci::finish_ci_run(f.pool(), failed.id, "worker", "failed", None)
        .await
        .unwrap()
        .unwrap();
    blocked(&alice, main_number, "Required status checks not passing").await;

    // The newer run belongs to the other pull request, and it passes.
    let passed = enqueue_pull_request_run(
        f.pool(),
        repository.id,
        owner,
        to_release["id"].as_i64().unwrap(),
        "release",
        &head,
    )
    .await;
    let claimed = db::ci::claim_ci_run(f.pool(), "worker", "local", &[owner])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, passed.id);
    db::ci::finish_ci_run(f.pool(), passed.id, "worker", "success", None)
        .await
        .unwrap()
        .unwrap();
    blocked(&alice, main_number, "Required status checks not passing").await;
    let merged = alice.merge_current(release_number).await;
    assert_eq!(merged["merged"], true, "{merged}");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_runs_of_another_pull_request_do_not_satisfy_a_check() {
    runs_of_another_pull_request_do_not_satisfy_a_check(false).await;
}

#[tokio::test]
async fn postgres_runs_of_another_pull_request_do_not_satisfy_a_check() {
    runs_of_another_pull_request_do_not_satisfy_a_check(true).await;
}

/// A failed run on the head commit makes the merge box say so, and the run is listed under the
/// pull request's head branch even though it is stored on the base branch.
async fn pull_request_checks_show_failures_and_the_head_branch(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let state = Arc::new(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let router = create_router((*state).clone());
    let alice = Client::login(&router, f.pool(), owner).await;
    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic", &base).await;
    let head = alice.save("topic", "topic.txt", &base).await;
    let pull = alice.open_pull("To main", "topic", "main").await;
    let number = pull["number"].as_i64().unwrap();
    let checks_path = format!("/alice/proj/pulls/{number}/checks");

    let run = enqueue_pull_request_run(
        f.pool(),
        repository.id,
        owner,
        pull["id"].as_i64().unwrap(),
        "main",
        &head,
    )
    .await;
    let claimed = db::ci::claim_ci_run(f.pool(), "worker", "local", &[owner])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, run.id);
    db::ci::finish_ci_run(f.pool(), run.id, "worker", "failed", None)
        .await
        .unwrap()
        .unwrap();

    // The merge stays allowed: the box shows the failure and links to the Checks tab.
    let (status, page) = alice.page(&format!("/alice/proj/pulls/{number}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Some checks were not successful"), "{page}");
    assert!(page.contains("1 check failed."), "{page}");
    assert!(page.contains(&format!("href=\"{checks_path}\"")), "{page}");
    assert!(!page.contains("Ready to merge"), "{page}");
    assert!(!page.contains("Merging is blocked"), "{page}");

    let (status, checks) = alice.page(&checks_path).await;
    assert_eq!(status, StatusCode::OK);
    assert!(checks.contains(", topic, run "), "{checks}");
    assert!(!checks.contains(", main, run "), "{checks}");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_pull_request_checks_show_failures_and_the_head_branch() {
    pull_request_checks_show_failures_and_the_head_branch(false).await;
}

#[tokio::test]
async fn postgres_pull_request_checks_show_failures_and_the_head_branch() {
    pull_request_checks_show_failures_and_the_head_branch(true).await;
}

async fn merge_with(client: &Client, number: i64, method: &str) -> (StatusCode, serde_json::Value) {
    let pull = client.pull(number).await;
    client
        .api(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/merge"),
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"], "merge_method": method}),
        )
        .await
}

/// Each merge method under the two history rules. A head that does not contain the base is out
/// of date, and the up-to-date rule stops it for every method. The merge method always makes a
/// merge commit, so linear history stops that method and nothing else.
async fn merge_method_history_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute(
        "INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE",
        &[],
    )
    .await;
    let owner = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let state = Arc::new(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let router = create_router((*state).clone());
    let alice = Client::login(&router, f.pool(), owner).await;
    let mut main = alice.save("main", "README.md", "unborn").await;
    for (rule, up_to_date, linear) in [("up-to-date", true, false), ("linear", false, true)] {
        // The rule applies to the owner too, and direct saves to main stay possible for the test.
        db::repositories::protection::upsert_branch_protection_rule(
            f.pool(),
            db::repositories::protection::BranchProtectionRuleUpsert {
                actor_id: owner,
                repo_id: repository.id,
                pattern: "main",
                require_pull_request: false,
                require_up_to_date_branch: up_to_date,
                require_linear_history: linear,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        for method in ["merge", "squash", "rebase"] {
            // The base moves on after the head branches off, so the head is out of date.
            let behind = format!("{rule}-behind-{method}");
            alice.branch(&behind, &main).await;
            alice.save(&behind, &format!("{behind}.txt"), &main).await;
            let number = alice.open_pull(&behind, &behind, "main").await["number"]
                .as_i64()
                .unwrap();
            main = alice
                .save("main", &format!("{behind}-base.txt"), &main)
                .await;
            let (status, answer) = merge_with(&alice, number, method).await;
            if up_to_date || (linear && method == "merge") {
                let refusal = if up_to_date {
                    "out of date"
                } else {
                    "Merge commits are not allowed"
                };
                assert_eq!(status, StatusCode::CONFLICT, "{rule} {method}: {answer}");
                assert_eq!(
                    answer["error"], "branch_protection",
                    "{rule} {method}: {answer}"
                );
                assert!(
                    text(&answer, "message").contains(refusal),
                    "{rule} {method}: {answer}"
                );
                assert_eq!(alice.pull(number).await["state"], "open");
            } else {
                assert_eq!(status, StatusCode::OK, "{rule} {method}: {answer}");
                main = text(&answer, "sha").to_owned();
            }
        }
        // A head that already contains the base is up to date. The merge method still makes a
        // merge commit of the base and the head, and linear history refuses only that method.
        for method in ["merge", "squash", "rebase"] {
            let fresh = format!("{rule}-fresh-{method}");
            alice.branch(&fresh, &main).await;
            let fresh_head = alice.save(&fresh, &format!("{fresh}.txt"), &main).await;
            let number = alice.open_pull(&fresh, &fresh, "main").await["number"]
                .as_i64()
                .unwrap();
            let before = main.clone();
            let (status, answer) = merge_with(&alice, number, method).await;
            if linear && method == "merge" {
                assert_eq!(status, StatusCode::CONFLICT, "{rule} {method}: {answer}");
                assert!(
                    text(&answer, "message").contains("Merge commits are not allowed"),
                    "{rule} {method}: {answer}"
                );
                assert_eq!(alice.pull(number).await["state"], "open");
                continue;
            }
            assert_eq!(status, StatusCode::OK, "{rule} {method}: {answer}");
            main = text(&answer, "sha").to_owned();
            if method == "merge" {
                let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
                let landed = git
                    .find_commit(git2::Oid::from_str(&main).unwrap())
                    .unwrap();
                assert_eq!(
                    landed.parent_ids().collect::<Vec<_>>(),
                    vec![
                        git2::Oid::from_str(&before).unwrap(),
                        git2::Oid::from_str(&fresh_head).unwrap()
                    ]
                );
            }
        }
    }
    f.finish().await;
}

#[tokio::test]
async fn sqlite_merge_methods_follow_the_history_rules() {
    merge_method_history_contract(false).await;
}

#[tokio::test]
async fn postgres_merge_methods_follow_the_history_rules() {
    merge_method_history_contract(true).await;
}
