use super::*;
use codeyard_app::ci::config::CiExecutionMode;
use codeyard_app::ci::queue::RunQueue;
use codeyard_app::repositories::lifecycle;
use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use std::collections::BTreeSet;

async fn contract(pg: bool) {
    use base64::Engine;

    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("alice").await;
    let outsider = f.actor("outsider").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let config = CiConfig {
        trusted_actor_ids: BTreeSet::from([owner]),
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
    let writer = credential(&router, &f, owner, "write").await;
    let reader = credential(&router, &f, owner, "read").await;
    let outsider = credential(&router, &f, outsider, "write").await;
    let path = "/api/v3/repos/alice/proj/actions/permissions";
    let get = "actions/get-github-actions-permissions-repository";
    let put = "actions/set-github-actions-permissions-repository";
    let (_, defaults) = checked(
        &router,
        get,
        "GET",
        path,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        defaults,
        json!({"enabled":true,"allowed_actions":"all","sha_pinning_required":false})
    );
    for (method, operation, token, status) in [
        ("GET", get, None, StatusCode::UNAUTHORIZED),
        ("GET", get, Some(&outsider), StatusCode::FORBIDDEN),
        ("PUT", put, Some(&outsider), StatusCode::FORBIDDEN),
        ("PUT", put, Some(&reader), StatusCode::FORBIDDEN),
    ] {
        checked(
            &router,
            operation,
            method,
            path,
            token.map(String::as_str),
            json!({"enabled":false}),
            status,
        )
        .await;
    }
    for body in [
        json!({}),
        json!({"enabled":"false"}),
        json!({"enabled":false,"allowed_actions":"selected"}),
        json!({"enabled":false,"sha_pinning_required":true}),
    ] {
        checked(
            &router,
            put,
            "PUT",
            path,
            Some(&writer),
            body,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let client = Client::login(&router, f.pool(), owner).await;
    let id = repo.id.to_string();
    let native_path = "/alice/proj/_forms/actions-settings";
    let form = [
        ("repository_id", id.as_str()),
        ("enabled", "on"),
        ("run_timeout_minutes", "10"),
        ("job_cpus", "2"),
        ("job_memory_gib", "3"),
        ("cache_size_gib", "1"),
        ("cache_retention_days", "2"),
        ("engine", "act"),
    ];
    assert_eq!(
        client.form_reply(native_path, &form).await.status,
        StatusCode::SEE_OTHER
    );
    let saved = codeyard_db_actions::actions::get_repo_actions_settings(f.pool(), repo.id)
        .await
        .unwrap();
    let workflow = base64::engine::general_purpose::STANDARD.encode("name: Manual\non: workflow_dispatch\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo test\n");
    client.expect("POST", "/api/repos/alice/proj/contents/.github/workflows/ci.yml", json!({"content":workflow,"message":"Workflow","branch":"main","expected_revision":"unborn"}), StatusCode::CREATED).await;
    let dispatch = [
        ("branch", "main"),
        ("workflow_path", ".github/workflows/ci.yml"),
    ];
    assert_eq!(
        client
            .form_reply("/alice/proj/actions/dispatch", &dispatch)
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    checked(
        &router,
        put,
        "PUT",
        path,
        Some(&writer),
        json!({"enabled":false,"allowed_actions":"all","sha_pinning_required":false}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let mut disabled = saved.clone();
    disabled.enabled = false;
    assert_eq!(
        codeyard_db_actions::actions::get_repo_actions_settings(f.pool(), repo.id)
            .await
            .unwrap(),
        disabled
    );
    let page = client.page_ok("/alice/proj/settings?tab=actions").await;
    assert!(page.contains("value=\"10\""), "{page}");
    assert!(page.contains("name=\"enabled\""), "{page}");
    assert!(
        !codeyard_db_actions::actions::get_repo_actions_settings(f.pool(), repo.id)
            .await
            .unwrap()
            .enabled
    );
    assert_eq!(
        client
            .form_reply("/alice/proj/actions/dispatch", &dispatch)
            .await
            .status,
        StatusCode::CONFLICT
    );
    let queue = RunQueue::Database {
        pool: f.pool().clone(),
        config: Arc::new(config),
        secrets: None,
        context: None,
        notifications: Arc::new(NotificationBroadcaster::new(f.pool())),
    };
    assert!(queue
        .claim(
            "slot-1",
            CiExecutionMode::Local,
            &codeyard_app::ci::config::CiConfig::default().platform_labels()
        )
        .await
        .unwrap()
        .is_none());
    let runs = codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "cancelled");
    checked(
        &router,
        put,
        "PUT",
        path,
        Some(&writer),
        json!({"enabled":true}),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        codeyard_db_actions::actions::get_repo_actions_settings(f.pool(), repo.id)
            .await
            .unwrap(),
        saved
    );
    assert_eq!(
        client
            .form_reply("/alice/proj/actions/dispatch", &dispatch)
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    assert!(queue
        .claim(
            "slot-1",
            CiExecutionMode::Local,
            &codeyard_app::ci::config::CiConfig::default().platform_labels()
        )
        .await
        .unwrap()
        .is_some());
    // Native forms and REST edit the same switch, not copies of execution policy.
    let disabled_form: Vec<_> = form
        .iter()
        .copied()
        .filter(|(key, _)| *key != "enabled")
        .collect();
    assert_eq!(
        client.form_reply(native_path, &disabled_form).await.status,
        StatusCode::SEE_OTHER
    );
    let (_, value) = checked(
        &router,
        get,
        "GET",
        path,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(value["enabled"], false);
    f.execute(
        "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
        &[repo.id],
    )
    .await;
    checked(
        &router,
        get,
        "GET",
        path,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        put,
        "PUT",
        path,
        Some(&writer),
        json!({"enabled":true}),
        StatusCode::FORBIDDEN,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn repository_action_permissions_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn repository_action_permissions_postgres() {
    contract(true).await;
}
