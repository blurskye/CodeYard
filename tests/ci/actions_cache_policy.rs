//! Administrative cache limits patch one field and constrain existing and future runtime objects.
use super::{actions_objects::pat, results};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use codeyard::backend::db;
use codeyard_app::ci;
use codeyard_db_actions::action_objects as objects;
use codeyard_db_actions::action_objects::NewObject;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tower::ServiceExt;

async fn checked(
    router: &Router,
    kind: &str,
    method: &str,
    token: &str,
    body: Value,
    expected: StatusCode,
) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(format!(
                    "/api/v3/repos/alice/project/actions/cache/{kind}-limit"
                ))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(status, expected, "{}", String::from_utf8_lossy(&bytes));
    if status == StatusCode::NO_CONTENT {
        assert!(bytes.is_empty());
        return Value::Null;
    }
    let value = serde_json::from_slice(&bytes).unwrap();
    assert_github_schema(
        &format!(
            "actions/{}-actions-cache-{kind}-limit-for-repository",
            if method == "GET" { "get" } else { "set" }
        ),
        status.as_u16(),
        &value,
    );
    value
}

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    f.execute(
        "INSERT INTO repo_access(user_id,repo_id,access_level) VALUES($1,$2,3)",
        &[bob, repo.id],
    )
    .await;
    db::actions::save_repo_actions_settings(
        f.pool(),
        alice,
        repo.id,
        &db::actions::RepoActionsSettings {
            enabled: false,
            engine: Some("act".to_owned()),
            run_timeout_minutes: Some(17),
            job_cpus: Some(2),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let writer = pat(&f, &router, alice, "write").await;
    let reader = pat(&f, &router, alice, "read").await;
    let collaborator = pat(&f, &router, bob, "write").await;
    assert_eq!(
        checked(
            &router,
            "storage",
            "GET",
            &reader,
            Value::Null,
            StatusCode::OK
        )
        .await["max_cache_size_gb"],
        10
    );
    assert_eq!(
        checked(
            &router,
            "retention",
            "GET",
            &reader,
            Value::Null,
            StatusCode::OK
        )
        .await["max_cache_retention_days"],
        7
    );
    for method in ["GET", "PUT"] {
        checked(
            &router,
            "storage",
            method,
            &collaborator,
            json!({"max_cache_size_gb":1}),
            StatusCode::FORBIDDEN,
        )
        .await;
    }
    checked(
        &router,
        "storage",
        "PUT",
        &reader,
        json!({"max_cache_size_gb":1}),
        StatusCode::FORBIDDEN,
    )
    .await;
    for value in [-1, 0, 11] {
        checked(
            &router,
            "storage",
            "PUT",
            &writer,
            json!({"max_cache_size_gb":value}),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    checked(
        &router,
        "retention",
        "PUT",
        &writer,
        json!({"max_cache_retention_days":8}),
        StatusCode::BAD_REQUEST,
    )
    .await;
    checked(
        &router,
        "retention",
        "PUT",
        &writer,
        json!({}),
        StatusCode::BAD_REQUEST,
    )
    .await;
    let (run, runtime) = results::run(&f, repo.id, alice, "main").await;
    let cache = results::cache(&router, &runtime.token, "before", b"cache").await;
    checked(
        &router,
        "retention",
        "PUT",
        &writer,
        json!({"max_cache_retention_days":3}),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT expires_at-accessed_at FROM actions_objects WHERE id=$1",
            &[cache]
        )
        .await,
        3 * 86400
    );
    let credentials = objects::authenticate(f.pool(), &ci::runtime::token_hash(&runtime.token))
        .await
        .unwrap()
        .unwrap();
    let now = chrono::Utc::now().timestamp();
    let stale = objects::create_object(
        f.pool(),
        NewObject {
            run: &credentials,
            kind: "cache",
            name: "stale-policy",
            version: "v1",
            expires_at: now + 7 * 86400,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        stale.expires_at <= now + 3 * 86400,
        "a stale preflight must not escape the current policy"
    );
    tokio::join!(
        checked(
            &router,
            "storage",
            "PUT",
            &writer,
            json!({"max_cache_size_gb":1}),
            StatusCode::NO_CONTENT
        ),
        checked(
            &router,
            "retention",
            "PUT",
            &writer,
            json!({"max_cache_retention_days":2}),
            StatusCode::NO_CONTENT
        )
    );
    assert_eq!(
        checked(
            &router,
            "storage",
            "GET",
            &reader,
            Value::Null,
            StatusCode::OK
        )
        .await["max_cache_size_gb"],
        1
    );
    assert_eq!(
        checked(
            &router,
            "retention",
            "GET",
            &reader,
            Value::Null,
            StatusCode::OK
        )
        .await["max_cache_retention_days"],
        2
    );
    let own = db::actions::get_repo_actions_settings(f.pool(), repo.id)
        .await
        .unwrap();
    assert!(!own.enabled);
    assert_eq!(own.engine.as_deref(), Some("act"));
    assert_eq!(own.job_cpus, Some(2));
    assert_eq!(own.run_timeout_minutes, Some(17));
    assert_eq!(
        f.scalar(
            "SELECT expires_at-accessed_at FROM actions_objects WHERE id=$1",
            &[cache]
        )
        .await,
        2 * 86400
    );
    let after = results::cache(&router, &runtime.token, "after", b"new cache").await;
    assert_eq!(
        f.scalar(
            "SELECT expires_at-accessed_at FROM actions_objects WHERE id=$1",
            &[after]
        )
        .await,
        2 * 86400
    );
    let jobs = ci::tokens::issue(
        f.pool(),
        &run,
        &BTreeMap::from([(
            ".github/workflows/ci.yml".to_owned(),
            b"jobs:\n  job:\n    permissions:\n      actions: write\n".to_vec(),
        )]),
        false,
    )
    .await
    .unwrap();
    checked(
        &router,
        "storage",
        "GET",
        &jobs.workflows[".github/workflows/ci.yml"]["job"],
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    let page = Client::login(&router, f.pool(), alice)
        .await
        .page_ok("/alice/project/settings?tab=actions")
        .await;
    assert!(page.contains("value=\"2\""));
    assert_eq!(f.scalar("SELECT COUNT(*) FROM audit_log WHERE action='repository.actions_cache_policy' AND target_id=$1",&[repo.id]).await,3);
    // A native settings save applies its cache retention to the stored caches as well.
    db::actions::save_repo_actions_settings(
        f.pool(),
        alice,
        repo.id,
        &db::actions::RepoActionsSettings {
            engine: Some("act".to_owned()),
            cache_size_gib: Some(1),
            cache_retention_days: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        f.scalar(
            "SELECT expires_at-accessed_at FROM actions_objects WHERE id=$1",
            &[after]
        )
        .await,
        86400
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_actions_cache_policy() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_actions_cache_policy() {
    contract(true).await;
}
