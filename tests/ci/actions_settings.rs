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
    let site_form = [
        ("enabled", "on"),
        ("run_timeout_minutes", "30"),
        ("job_cpus", "4"),
        ("job_memory_gib", "8"),
        ("runner_image", ""),
    ];
    let reply = alice.form_reply("/admin/actions", &site_form).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());
    let reply = admin
        .form_reply("/admin/actions", &[("enabled", "on"), ("job_cpus", "0")])
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    assert!(reply.text().contains("from 1 to 1024"), "{}", reply.text());
    let reply = admin.form_reply("/admin/actions", &site_form).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let page = admin.page_ok("/admin/actions?saved=1").await;
    assert!(page.contains("Actions settings saved."), "{page}");
    assert!(page.contains("value=\"30\""), "{page}");

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
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let page = alice.page_ok("/alice/proj/settings?tab=actions").await;
    assert!(page.contains("value=\"4\""), "{page}");

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
        .claim("slot-1", CiExecutionMode::Local)
        .await
        .unwrap()
        .expect("the dispatched run");
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
