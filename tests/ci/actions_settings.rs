//! The Actions settings: an administrator sets the site's limits on each run, a repository may
//! only lower them, a claimed run carries the stricter of the two, and turning Actions off stops
//! new runs. On SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database::Database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client::Client;
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::{queue::RunQueue, CiConfig, CiExecutionMode, RunLimits},
    db,
    notifications::NotificationBroadcaster,
    storage::ObjectStore,
};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;

async fn contract(pg: bool) {
    use base64::Engine;

    bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let admin_id = f.actor("admin").await;
    let alice_id = f.actor("alice").await;
    f.execute(
        "UPDATE users SET is_admin = TRUE WHERE id = $1",
        &[admin_id],
    )
    .await;
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
    let config = CiConfig {
        trusted_actor_ids: BTreeSet::from([alice_id]),
        ..CiConfig::default()
    };
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        config.clone(),
        None,
    ));
    let admin = Client::login(&router, f.pool(), admin_id).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;

    // Only an administrator sees or changes the site settings.
    let (status, _) = alice.page("/admin/actions").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let defaults = db::actions::get_actions_settings(f.pool()).await.unwrap();
    assert_eq!(defaults.default_engine, "codeyard-actions");
    assert_eq!(defaults.default_workflow_permissions, "restricted");
    let refused = admin
        .form_reply("/admin/actions", &[("default_engine", "act")])
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert!(refused.text().contains("at least one engine"));
    let site_form = [
        ("enabled", "on"),
        ("default_engine", "act"),
        ("codeyard_actions_enabled", "on"),
        ("act_enabled", "on"),
        ("default_workflow_permissions", "permissive"),
        ("run_timeout_minutes", "30"),
        ("job_cpus", "4"),
        ("job_memory_gib", "8"),
        ("runner_image", ""),
        ("results_url", "http://ci.example:3000"),
        ("cache_size_gib", "2"),
        ("cache_retention_days", "4"),
        ("artifact_retention_days", "20"),
    ];
    let reply = alice.form_reply("/admin/actions", &site_form).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());
    let reply = admin
        .form_reply("/admin/actions", &[("enabled", "on"), ("job_cpus", "0")])
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    assert!(reply.text().contains("from 1 to 1024"), "{}", reply.text());
    let reply = admin
        .form_reply(
            "/admin/actions",
            &[
                ("enabled", "on"),
                ("results_url", "https://ci.example/path"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = admin.form_reply("/admin/actions", &site_form).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let page = admin.page_ok("/admin/actions?saved=1").await;
    assert!(page.contains("Actions settings saved."), "{page}");
    assert!(page.contains("value=\"30\""), "{page}");
    assert!(page.contains("http://ci.example:3000"), "{page}");
    let saved = db::actions::get_actions_settings(f.pool()).await.unwrap();
    assert_eq!(saved.cache_size_gib, Some(2));
    assert_eq!(saved.cache_retention_days, Some(4));
    assert_eq!(saved.artifact_retention_days, Some(20));
    assert_eq!(saved.default_engine, "act");
    assert_eq!(saved.default_workflow_permissions, "permissive");

    // A repository may lower a limit, not raise it.
    let rid = repo.id.to_string();
    let reply = alice
        .form_reply(
            "/alice/proj/_forms/actions-settings",
            &[
                ("repository_id", &rid),
                ("enabled", "on"),
                ("job_memory_gib", "16"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    assert!(reply.text().contains("at most 8"), "{}", reply.text());
    let reply = alice
        .form_reply(
            "/alice/proj/_forms/actions-settings",
            &[
                ("repository_id", &rid),
                ("enabled", "on"),
                ("job_memory_gib", "4"),
                ("cache_size_gib", "1"),
                ("cache_retention_days", "3"),
                ("engine", "codeyard-actions"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let page = alice.page_ok("/alice/proj/settings?tab=actions").await;
    assert!(page.contains("value=\"4\""), "{page}");
    let own = db::actions::get_repo_actions_settings(f.pool(), repo.id)
        .await
        .unwrap();
    assert_eq!(saved.cache_size_bytes(&own), 1024 * 1024 * 1024);
    assert_eq!(saved.cache_expiry_days(&own), 3);
    assert_eq!(own.engine.as_deref(), Some("codeyard-actions"));
    let fallback_form: Vec<_> = site_form
        .iter()
        .copied()
        .filter(|(key, _)| *key != "codeyard_actions_enabled")
        .collect();
    let reply = admin.form_reply("/admin/actions", &fallback_form).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let page = alice.page_ok("/alice/proj/settings?tab=actions").await;
    assert!(page.contains("engine is disabled"), "{page}");
    let reply = alice
        .form_reply(
            "/alice/proj/_forms/actions-settings",
            &[
                ("repository_id", &rid),
                ("enabled", "on"),
                ("engine", "codeyard-actions"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

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
    let dispatch = [
        ("branch", "main"),
        ("workflow_path", ".github/workflows/ci.yml"),
    ];
    let reply = alice
        .form_reply("/alice/proj/actions/dispatch", &dispatch)
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let queue = RunQueue::Database {
        pool: f.pool().clone(),
        config: Arc::new(config),
        secrets: None,
        context: None,
        notifications: Arc::new(NotificationBroadcaster::new(f.pool())),
    };
    let claimed = queue
        .claim(
            "slot-1",
            CiExecutionMode::Local,
            &codeyard::backend::ci::CiConfig::default().platform_labels(),
        )
        .await
        .unwrap()
        .expect("the dispatched run");
    assert_eq!(claimed.runtime.results_url, "http://ci.example:3000");
    assert_eq!(claimed.run.engine.as_deref(), Some("act"));
    assert_eq!(
        db::action_objects::authenticate(
            f.pool(),
            &codeyard::backend::ci::runtime::token_hash(&claimed.runtime.token)
        )
        .await
        .unwrap()
        .unwrap()
        .id,
        claimed.run.id
    );
    assert_eq!(
        claimed.limits,
        RunLimits {
            timeout_seconds: Some(1800),
            cpus: Some(4),
            memory_gib: Some(4),
            image: None,
        }
    );

    // With Actions off for the repository, a dispatch is refused and nothing is queued.
    let reply = alice
        .form_reply(
            "/alice/proj/_forms/actions-settings",
            &[("repository_id", &rid), ("job_memory_gib", "4")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let reply = alice
        .form_reply("/alice/proj/actions/dispatch", &dispatch)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text());
    assert!(reply.text().contains("turned off"), "{}", reply.text());
    assert_eq!(
        db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn sqlite_actions_settings_limit_and_stop_runs() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_actions_settings_limit_and_stop_runs() {
    contract(true).await;
}

async fn rest_settings(pg: bool) {
    use super::{
        actions_objects::{artifact, pat},
        actions_workflows::{checked, router},
        results,
    };
    use crate::support::organizations::applied;
    use db::organizations::{NewOrganization, OrganizationProfile};
    use db::repositories::creation::{RepositoryCreationOwner, RepositoryTarget};
    use serde_json::Value;

    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let org = applied(
        db::organizations::create_organization(
            f.pool(),
            alice,
            &NewOrganization::new("builders", OrganizationProfile::new(None, None).unwrap())
                .unwrap(),
        )
        .await
        .unwrap(),
    );
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'member')",
        &[org.id, bob],
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner {
            actor_id: alice,
            target: RepositoryTarget::Organization(org.id),
        },
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let api = router(&f, root.path(), alice).await;
    let writer = pat(&f, &api, alice, "write").await;
    let reader = pat(&f, &api, alice, "read").await;
    let member = pat(&f, &api, bob, "write").await;
    let repo_base = "/api/v3/repos/builders/project/actions";
    let org_base = "/api/v3/orgs/builders/actions";
    let groups = [
        (
            "allowed-actions",
            "selected-actions",
            json!({"github_owned_allowed":true,"verified_allowed":true,"patterns_allowed":["actions/*","builders/*@*"]}),
        ),
        (
            "artifact-and-log-retention-settings",
            "artifact-and-log-retention",
            json!({"days":12}),
        ),
        (
            "fork-pr-contributor-approval-permissions",
            "fork-pr-contributor-approval",
            json!({"approval_policy":"all_external_contributors"}),
        ),
        (
            "github-actions-default-workflow-permissions",
            "workflow",
            json!({"default_workflow_permissions":"write","can_approve_pull_request_reviews":true}),
        ),
        (
            "private-repo-fork-pr-workflows-settings",
            "fork-pr-workflows-private-repos",
            json!({"run_workflows_from_fork_pull_requests":true,"send_write_tokens_to_workflows":false,"send_secrets_and_variables":false,"require_approval_for_fork_pr_workflows":true}),
        ),
    ];
    for (base, scope) in [(org_base, "organization"), (repo_base, "repository")] {
        for (group, suffix, input) in &groups {
            let path = format!("{base}/permissions/{suffix}");
            let get = format!("actions/get-{group}-{scope}");
            let set = format!("actions/set-{group}-{scope}");
            checked(
                &api,
                &get,
                "GET",
                &path,
                &reader,
                Value::Null,
                StatusCode::OK,
            )
            .await;
            checked(
                &api,
                &set,
                "PUT",
                &path,
                &writer,
                input.clone(),
                StatusCode::NO_CONTENT,
            )
            .await;
            let (_, saved) = checked(
                &api,
                &get,
                "GET",
                &path,
                &reader,
                Value::Null,
                StatusCode::OK,
            )
            .await;
            for (key, value) in input.as_object().unwrap() {
                assert_eq!(&saved[key], value, "{path}: {saved}");
            }
            checked(
                &api,
                &get,
                "GET",
                &path,
                &member,
                Value::Null,
                StatusCode::FORBIDDEN,
            )
            .await;
            checked(
                &api,
                &set,
                "PUT",
                &path,
                &member,
                input.clone(),
                StatusCode::FORBIDDEN,
            )
            .await;
            checked(
                &api,
                &set,
                "PUT",
                &path,
                &reader,
                input.clone(),
                StatusCode::FORBIDDEN,
            )
            .await;
        }
    }
    for (group, suffix, input, status) in [
        (
            "custom-oidc-sub-claim-for-repo",
            "oidc/customization/sub",
            json!({"use_default":false,"include_claim_keys":["repo","context","job_workflow_ref"]}),
            StatusCode::CREATED,
        ),
        (
            "workflow-access-to-repository",
            "permissions/access",
            json!({"access_level":"organization"}),
            StatusCode::NO_CONTENT,
        ),
    ] {
        let path = format!("{repo_base}/{suffix}");
        let get = format!("actions/get-{group}");
        let set = format!("actions/set-{group}");
        checked(&api, &set, "PUT", &path, &writer, input.clone(), status).await;
        let (_, saved) = checked(
            &api,
            &get,
            "GET",
            &path,
            &reader,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(saved, input);
        checked(
            &api,
            &get,
            "GET",
            &path,
            &member,
            Value::Null,
            StatusCode::FORBIDDEN,
        )
        .await;
        checked(
            &api,
            &set,
            "PUT",
            &path,
            &member,
            input.clone(),
            StatusCode::FORBIDDEN,
        )
        .await;
        checked(
            &api,
            &set,
            "PUT",
            &path,
            &reader,
            input,
            StatusCode::FORBIDDEN,
        )
        .await;
    }
    // Partial updates preserve unrelated fields, and invalid input cannot replace them.
    checked(
        &api,
        "actions/set-allowed-actions-repository",
        "PUT",
        &format!("{repo_base}/permissions/selected-actions"),
        &writer,
        json!({"verified_allowed":false}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let (_, saved) = checked(
        &api,
        "actions/get-allowed-actions-repository",
        "GET",
        &format!("{repo_base}/permissions/selected-actions"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(saved["github_owned_allowed"], true);
    assert_eq!(saved["patterns_allowed"], groups[0].2["patterns_allowed"]);
    for (suffix, input) in [
        ("artifact-and-log-retention", json!({"days":13})),
        ("artifact-and-log-retention", json!({"days":0})),
        ("workflow", json!({"default_workflow_permissions":"admin"})),
        ("selected-actions", json!({"patterns_allowed":[42]})),
        (
            "fork-pr-contributor-approval",
            json!({"approval_policy":"everyone"}),
        ),
    ] {
        checked(
            &api,
            "actions/set-allowed-actions-repository",
            "PUT",
            &format!("{repo_base}/permissions/{suffix}"),
            &writer,
            input,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let (_, retained) = checked(
        &api,
        "actions/get-artifact-and-log-retention-settings-repository",
        "GET",
        &format!("{repo_base}/permissions/artifact-and-log-retention"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(retained, json!({"days":12,"maximum_allowed_days":12}));
    for (base, scope, source) in [
        (org_base, "org", "builders"),
        (repo_base, "repo", "builders/project"),
    ] {
        let path = format!("{base}/policies");
        // Active restrictive policies are refused until CI admission enforces them.
        checked(
            &api,
            &format!("actions/create-{scope}-actions-policy"),
            "POST",
            &path,
            &writer,
            json!({"name":"Trusted events","enforcement":"active","rules":[{"type":"restrict_action_events","parameters":{"allowed_events":["push"]}}]}),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        let policy = json!({"name":"Trusted events","enforcement":"evaluate","rules":[{"type":"restrict_action_events","parameters":{"allowed_events":["push","workflow_dispatch"]}}]});
        checked(
            &api,
            &format!("actions/create-{scope}-actions-policy"),
            "POST",
            &path,
            &member,
            policy.clone(),
            StatusCode::FORBIDDEN,
        )
        .await;
        checked(
            &api,
            &format!("actions/create-{scope}-actions-policy"),
            "POST",
            &path,
            &reader,
            policy.clone(),
            StatusCode::FORBIDDEN,
        )
        .await;
        let (_, created) = checked(
            &api,
            &format!("actions/create-{scope}-actions-policy"),
            "POST",
            &path,
            &writer,
            policy.clone(),
            StatusCode::CREATED,
        )
        .await;
        assert_eq!(created["source"], source);
        assert_eq!(created["target"], "actions");
        let id = created["id"].as_i64().unwrap();
        let item = format!("{path}/{id}");
        let (_, listed) = checked(
            &api,
            &format!("actions/get-{scope}-actions-policies"),
            "GET",
            &format!("{path}?per_page=1"),
            &reader,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(listed["total_count"], 1);
        assert_eq!(listed["policies"][0]["id"], id);
        checked(
            &api,
            &format!("actions/get-{scope}-actions-policies"),
            "GET",
            &path,
            &member,
            Value::Null,
            StatusCode::FORBIDDEN,
        )
        .await;
        let (_, found) = checked(
            &api,
            &format!("actions/get-{scope}-actions-policy"),
            "GET",
            &item,
            &reader,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(found, created);
        checked(
            &api,
            &format!("actions/get-{scope}-actions-policy"),
            "GET",
            &item,
            &member,
            Value::Null,
            StatusCode::FORBIDDEN,
        )
        .await;
        checked(
            &api,
            &format!("actions/update-{scope}-actions-policy"),
            "PUT",
            &item,
            &member,
            json!({"name":"Denied"}),
            StatusCode::FORBIDDEN,
        )
        .await;
        let (_, updated) = checked(
            &api,
            &format!("actions/update-{scope}-actions-policy"),
            "PUT",
            &item,
            &writer,
            json!({"enforcement":"evaluate"}),
            StatusCode::OK,
        )
        .await;
        assert_eq!(updated["rules"], policy["rules"]);
        assert_eq!(updated["enforcement"], "evaluate");
        checked(
            &api,
            &format!("actions/get-{scope}-actions-policy"),
            "GET",
            &format!(
                "{}/policies/{id}",
                if scope == "org" { repo_base } else { org_base }
            ),
            &reader,
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
        checked(
            &api,
            &format!("actions/delete-{scope}-actions-policy"),
            "DELETE",
            &item,
            &member,
            Value::Null,
            StatusCode::FORBIDDEN,
        )
        .await;
        checked(
            &api,
            &format!("actions/delete-{scope}-actions-policy"),
            "DELETE",
            &item,
            &writer,
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
        checked(
            &api,
            &format!("actions/get-{scope}-actions-policy"),
            "GET",
            &item,
            &reader,
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    // The stored workflow default reaches the real queue's job credential issuance.
    {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        crate::support::production_ci::commit(&git, "refs/heads/main", None, "Build", None);
    }
    let queue = RunQueue::Database {
        pool: f.pool().clone(),
        config: Arc::new(CiConfig {
            trusted_actor_ids: BTreeSet::from([alice]),
            ..Default::default()
        }),
        secrets: None,
        context: None,
        notifications: Arc::new(NotificationBroadcaster::new(f.pool())),
    };
    for (permission, expected) in [("write", true), ("read", false)] {
        checked(
            &api,
            "actions/set-github-actions-default-workflow-permissions-repository",
            "PUT",
            &format!("{repo_base}/permissions/workflow"),
            &writer,
            json!({"default_workflow_permissions":permission}),
            StatusCode::NO_CONTENT,
        )
        .await;
        checked(
            &api,
            "actions/create-workflow-dispatch",
            "POST",
            &format!("{repo_base}/workflows/ci.yml/dispatches"),
            &writer,
            json!({"ref":"main"}),
            StatusCode::NO_CONTENT,
        )
        .await;
        let claim = queue
            .claim(
                "settings",
                CiExecutionMode::Local,
                &codeyard::backend::ci::CiConfig::default().platform_labels(),
            )
            .await
            .unwrap()
            .unwrap();
        let token = &claim.github_tokens.workflows[".github/workflows/ci.yml"]["test"];
        let pat = db::users::tokens::find_token(
            f.pool(),
            &codeyard::backend::ci::runtime::token_hash(token),
        )
        .await
        .unwrap()
        .unwrap();
        let token = db::ci_tokens::get(f.pool(), pat.id).await.unwrap().unwrap();
        let permissions: codeyard::backend::ci::tokens::Permissions =
            serde_json::from_str(&token.permissions).unwrap();
        assert_eq!(permissions.allows("contents", true), expected);
        db::ci::finish_ci_run(f.pool(), claim.run.id, "settings", "success", None)
            .await
            .unwrap()
            .unwrap();
    }
    // The organization switch decides which of its repositories may start runs.
    let switch = format!("{org_base}/permissions");
    let (_, value) = checked(
        &api,
        "actions/get-github-actions-permissions-organization",
        "GET",
        &switch,
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(value["enabled_repositories"], "all");
    assert!(value.get("selected_repositories_url").is_none());
    checked(
        &api,
        "actions/set-github-actions-permissions-organization",
        "PUT",
        &switch,
        &member,
        json!({"enabled_repositories":"none"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &api,
        "actions/set-github-actions-permissions-organization",
        "PUT",
        &switch,
        &writer,
        json!({"enabled_repositories":"selected","selected_repository_ids":[repo.id]}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &api,
        "actions/set-github-actions-permissions-organization",
        "PUT",
        &switch,
        &writer,
        json!({"enabled_repositories":"selected"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let dispatch = format!("{repo_base}/workflows/ci.yml/dispatches");
    checked(
        &api,
        "actions/create-workflow-dispatch",
        "POST",
        &dispatch,
        &writer,
        json!({"ref":"main"}),
        StatusCode::CONFLICT,
    )
    .await;
    checked(
        &api,
        "actions/enable-selected-repository-github-actions-organization",
        "PUT",
        &format!("{switch}/repositories/{}", repo.id),
        &writer,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let (_, listed) = checked(
        &api,
        "actions/list-selected-repositories-enabled-github-actions-organization",
        "GET",
        &format!("{switch}/repositories"),
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(listed["total_count"], 1);
    assert_eq!(listed["repositories"][0]["full_name"], "builders/project");
    let (_, value) = checked(
        &api,
        "actions/get-github-actions-permissions-organization",
        "GET",
        &switch,
        &reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(value["enabled_repositories"], "selected");
    assert!(value.get("selected_repository_ids").is_none());
    assert!(value["selected_repositories_url"]
        .as_str()
        .is_some_and(|url| url.ends_with("/orgs/builders/actions/permissions/repositories")));
    let queued = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 100)
        .await
        .unwrap()
        .len();
    checked(
        &api,
        "actions/create-workflow-dispatch",
        "POST",
        &dispatch,
        &writer,
        json!({"ref":"main"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 100)
            .await
            .unwrap()
            .len(),
        queued + 1
    );
    let claim = queue
        .claim(
            "settings",
            CiExecutionMode::Local,
            &codeyard::backend::ci::CiConfig::default().platform_labels(),
        )
        .await
        .unwrap()
        .unwrap();
    db::ci::finish_ci_run(f.pool(), claim.run.id, "settings", "success", None)
        .await
        .unwrap()
        .unwrap();
    checked(
        &api,
        "actions/disable-selected-repository-github-actions-organization",
        "DELETE",
        &format!("{switch}/repositories/{}", repo.id),
        &writer,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &api,
        "actions/create-workflow-dispatch",
        "POST",
        &dispatch,
        &writer,
        json!({"ref":"main"}),
        StatusCode::CONFLICT,
    )
    .await;
    checked(
        &api,
        "actions/set-github-actions-permissions-organization",
        "PUT",
        &switch,
        &writer,
        json!({"enabled_repositories":"all"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let (run, runtime) = results::run(&f, repo.id, alice, "main").await;
    let mut zip = vec![0; 22];
    zip[..4].copy_from_slice(b"PK\x05\x06");
    let id = artifact(&api, &run, &runtime.token, "retained", &zip).await;
    let lifetime = f
        .scalar(
            "SELECT expires_at-accessed_at FROM actions_objects WHERE id=$1",
            &[id],
        )
        .await;
    assert!((11 * 86400..=12 * 86400).contains(&lifetime));
    f.finish().await;
}

#[tokio::test]
async fn sqlite_actions_settings_rest_round_trips() {
    rest_settings(false).await;
}
#[tokio::test]
async fn postgres_actions_settings_rest_round_trips() {
    rest_settings(true).await;
}

async fn rest_reviews(pg: bool) {
    use super::{actions_objects::pat, actions_workflows::checked, results};
    use db::repositories::creation::RepositoryCreationOwner;
    use serde_json::Value;
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let carol = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner::personal(alice),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let fork = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner::personal(bob),
        "fork",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    f.execute(
        "UPDATE repositories SET forked_from_id=$1 WHERE id=$2",
        &[repo.id, fork.id],
    )
    .await;
    for actor in [bob, carol] {
        db::repositories::access::grant_repo_access(
            f.pool(),
            actor,
            repo.id,
            db::models::AccessLevel::Read,
        )
        .await
        .unwrap();
    }
    let api = super::actions_workflows::router(&f, root.path(), alice).await;
    let writer = pat(&f, &api, alice, "write").await;
    let reader = pat(&f, &api, alice, "read").await;
    let reviewer = pat(&f, &api, bob, "write").await;
    let bystander = pat(&f, &api, carol, "write").await;
    let sha = "a".repeat(40);
    let pull = db::pull_requests::records::create_pull_request(
        f.pool(),
        db::pull_requests::records::NewPullRequest {
            repo_id: repo.id,
            head_repo_id: Some(fork.id),
            author_id: bob,
            issue: None,
            title: "Fork changes",
            body: "",
            head_branch: "feature",
            head_sha: &sha,
            base_branch: "main",
            base_sha: &sha,
            is_draft: false,
            maintainer_can_modify: false,
        },
    )
    .await
    .unwrap();
    let (run, _) = results::run(&f, repo.id, alice, "main").await;
    f.execute("UPDATE ci_runs SET event='pull_request',is_fork=true,approval_required=true,status='queued',pull_request_id=$1,head_sha='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' WHERE id=$2",&[pull.id,run.id]).await;
    let base = "/api/v3/repos/alice/project/actions/runs";
    let approve = format!("{base}/{}/approve", run.id);
    // A read-only token is refused for its scope; readers without write access do not
    // learn more about the run than an outsider would.
    for (token, status) in [
        (&reader, StatusCode::FORBIDDEN),
        (&reviewer, StatusCode::NOT_FOUND),
        (&bystander, StatusCode::NOT_FOUND),
    ] {
        checked(
            &api,
            "actions/approve-workflow-run",
            "POST",
            &approve,
            token,
            Value::Null,
            status,
        )
        .await;
    }
    // Approving a held fork run with a real head is covered in actions_approval.
    let (_,env) = checked(&api,"repos/create-or-update-environment","PUT","/api/v3/repos/alice/project/environments/production",&writer,json!({"wait_timer":30,"prevent_self_review":true,"reviewers":[{"type":"User","id":alice},{"type":"User","id":bob}]}),StatusCode::OK).await;
    let environment = env["id"].as_i64().unwrap();
    let (_, foreign) = checked(
        &api,
        "repos/create-or-update-environment",
        "PUT",
        "/api/v3/repos/bob/fork/environments/foreign",
        &reviewer,
        json!({"reviewers":[{"type":"User","id":bob}]}),
        StatusCode::OK,
    )
    .await;
    let foreign = foreign["id"].as_i64().unwrap();
    for state in ["approved", "rejected"] {
        let (deployment_run, _) = results::run(&f, repo.id, alice, "main").await;
        f.execute("UPDATE ci_runs SET environment='production',head_sha='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' WHERE id=$1",&[deployment_run.id]).await;
        f.execute(
            "INSERT INTO ci_deployments(repo_id,run_id,environment,environment_key) VALUES($1,$2,'production','production')",
            &[repo.id, deployment_run.id],
        )
        .await;
        let path = format!("{base}/{}/pending_deployments", deployment_run.id);
        let history = format!("{base}/{}/approvals", deployment_run.id);
        let (_, pending) = checked(
            &api,
            "actions/get-pending-deployments-for-run",
            "GET",
            &path,
            &reviewer,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(pending.as_array().unwrap().len(), 1);
        assert_eq!(pending[0]["environment"]["id"], environment);
        assert_eq!(pending[0]["wait_timer"], 30);
        assert_eq!(pending[0]["current_user_can_approve"], true);
        assert_eq!(pending[0]["reviewers"].as_array().unwrap().len(), 2);
        let (_, pending) = checked(
            &api,
            "actions/get-pending-deployments-for-run",
            "GET",
            &path,
            &writer,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(pending[0]["current_user_can_approve"], false);
        let input = json!({"environment_ids":[environment],"state":state,"comment":"Reviewed the deployment"});
        for token in [&reader, &writer, &bystander] {
            checked(
                &api,
                "actions/review-pending-deployments-for-run",
                "POST",
                &path,
                token,
                input.clone(),
                StatusCode::FORBIDDEN,
            )
            .await;
        }
        checked(&api,"actions/review-pending-deployments-for-run","POST",&path,&reviewer,json!({"environment_ids":[environment,foreign],"state":state,"comment":"All or nothing"}),StatusCode::CONFLICT).await;
        let (_, empty) = checked(
            &api,
            "actions/get-reviews-for-run",
            "GET",
            &history,
            &reader,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(empty, json!([]));
        let (_, deployments) = checked(
            &api,
            "actions/review-pending-deployments-for-run",
            "POST",
            &path,
            &reviewer,
            input.clone(),
            StatusCode::OK,
        )
        .await;
        assert_eq!(deployments.as_array().unwrap().len(), 1);
        assert_eq!(deployments[0]["environment"], "production");
        assert_eq!(deployments[0]["sha"], sha);
        checked(
            &api,
            "actions/review-pending-deployments-for-run",
            "POST",
            &path,
            &reviewer,
            input,
            StatusCode::CONFLICT,
        )
        .await;
        let (_, pending) = checked(
            &api,
            "actions/get-pending-deployments-for-run",
            "GET",
            &path,
            &reader,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(pending, json!([]));
        let (_, reviews) = checked(
            &api,
            "actions/get-reviews-for-run",
            "GET",
            &history,
            &reader,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(reviews[0]["state"], state);
        assert_eq!(reviews[0]["user"]["login"], "bob");
        assert_eq!(reviews[0]["comment"], "Reviewed the deployment");
        assert_eq!(reviews[0]["environments"][0]["id"], environment);
        checked(
            &api,
            "actions/get-pending-deployments-for-run",
            "GET",
            &format!(
                "/api/v3/repos/bob/fork/actions/runs/{}/pending_deployments",
                deployment_run.id
            ),
            &reviewer,
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    db::repositories::access::revoke_repo_access(f.pool(), bob, repo.id)
        .await
        .unwrap();
    checked(
        &api,
        "actions/get-reviews-for-run",
        "GET",
        &format!("{base}/{}/approvals", run.id),
        &reviewer,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn sqlite_actions_settings_run_reviews() {
    rest_reviews(false).await;
}
#[tokio::test]
async fn postgres_actions_settings_run_reviews() {
    rest_reviews(true).await;
}
