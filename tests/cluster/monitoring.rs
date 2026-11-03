use crate::support::database::Database;
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, identity::tokens::token_hash,
    storage::ObjectStore,
};
use std::{path::Path, sync::Arc};
use tower::ServiceExt;

async fn state(database: &Database, root: &Path) -> AppState {
    AppState::new(
        database.pool().clone(),
        root.to_owned(),
        Arc::new(ObjectStore::open(root, None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    )
}

async fn get(
    router: &Router,
    path: &str,
    authorization: Option<&str>,
) -> (StatusCode, String, String) {
    let mut request = Request::builder().uri(path);
    if let Some(value) = authorization {
        request = request.header(header::AUTHORIZATION, value);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (
        status,
        content_type,
        String::from_utf8(body.to_vec()).unwrap(),
    )
}

async fn token(database: &Database, user: i64) -> String {
    let value = format!("cy_pat_{}", crate::support::native_database::token());
    let session = crate::support::native_database::token();
    db::users::sessions::create_session(
        database.pool(),
        &session,
        user,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    db::users::tokens::create_token(
        database.pool(),
        user,
        &session,
        "monitoring",
        &token_hash(&value),
        "read",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    value
}

#[tokio::test]
async fn metrics_access_is_hidden_and_rechecks_administrator_tokens() {
    for pg in [false, true] {
        let mut database = Database::new(pg).await;
        database.complete_setup().await;
        let root = tempfile::tempdir().unwrap();
        let mut state = state(&database, root.path()).await;
        state.metrics_token_hash = Some(token_hash("scrape-secret"));
        let router = create_router(state);
        for auth in [
            None,
            Some("Bearer wrong"),
            Some("Basic c2NyYXBlLXNlY3JldA=="),
            Some("Bearer "),
        ] {
            assert_eq!(
                get(&router, "/metrics", auth).await.0,
                StatusCode::NOT_FOUND
            );
        }
        let (status, content_type, body) =
            get(&router, "/metrics", Some("Bearer scrape-secret")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "text/plain; version=0.0.4; charset=utf-8");
        assert!(body.contains("codeyard_ci_queued_runs 0\n"));
        assert!(body.contains("codeyard_ci_oldest_queued_seconds 0\n"));
        assert!(!body.contains("scrape-secret"));

        let user = database.actor("metrics-admin").await;
        let value = token(&database, user).await;
        let auth = format!("Bearer {value}");
        assert_eq!(
            get(&router, "/metrics", Some(&auth)).await.0,
            StatusCode::NOT_FOUND
        );
        database
            .execute("UPDATE users SET is_admin=true WHERE id=$1", &[user])
            .await;
        assert_eq!(
            get(&router, "/metrics", Some(&auth)).await.0,
            StatusCode::OK
        );
        database
            .execute("UPDATE users SET is_admin=false WHERE id=$1", &[user])
            .await;
        assert_eq!(
            get(&router, "/metrics", Some(&auth)).await.0,
            StatusCode::NOT_FOUND
        );
        database
            .execute(
                "UPDATE users SET is_admin=true, is_banned=true WHERE id=$1",
                &[user],
            )
            .await;
        assert_eq!(
            get(&router, "/metrics", Some(&auth)).await.0,
            StatusCode::NOT_FOUND
        );
        database
            .execute("UPDATE users SET is_banned=false WHERE id=$1", &[user])
            .await;
        database
            .execute(
                "DELETE FROM personal_access_tokens WHERE user_id=$1",
                &[user],
            )
            .await;
        assert_eq!(
            get(&router, "/metrics", Some(&auth)).await.0,
            StatusCode::NOT_FOUND
        );
        database.finish().await;
    }
}

#[tokio::test]
async fn metrics_aggregate_storage_replicas_queue_and_node_states() {
    for pg in [false, true] {
        let mut database = Database::new(pg).await;
        let pool = database.pool();
        let storage = vec!["storage".to_owned()];
        let primary =
            db::cluster::register_own_node(pool, &"a".repeat(64), "primary", &storage, "test")
                .await
                .unwrap();
        let replica =
            db::cluster::register_own_node(pool, &"b".repeat(64), "replica", &storage, "test")
                .await
                .unwrap();
        let web = db::cluster::register_own_node(
            pool,
            &"c".repeat(64),
            "web",
            &["web".to_owned()],
            "test",
        )
        .await
        .unwrap();
        database
            .execute("UPDATE nodes SET disk_free=8192 WHERE id=$1", &[primary.id])
            .await;
        database
            .execute(
                "UPDATE nodes SET status='pending' WHERE id=$1",
                &[replica.id],
            )
            .await;
        database
            .execute("UPDATE nodes SET status='revoked' WHERE id=$1", &[web.id])
            .await;
        let owner = database.actor("private-owner").await;
        let repo = db::repositories::records::create_repository(
            pool,
            owner,
            "secret-repo",
            None,
            true,
            "/secret/path",
        )
        .await
        .unwrap();
        database.execute("UPDATE repositories SET size_bytes=512, write_version=9, storage_node_id=$1 WHERE id=$2", &[primary.id, repo.id]).await;
        database.execute("INSERT INTO repository_replicas(repository_id,node_id,applied_version) VALUES($1,$2,6)", &[repo.id, replica.id]).await;
        let insert = if pg {
            "INSERT INTO ci_runs(run_id,repo_id,mode,event,branch,workflow_path,created_at) VALUES('00000000-0000-0000-0000-000000000001',$1,'local','push','main','ci.yml',CURRENT_TIMESTAMP - INTERVAL '120 seconds')"
        } else {
            "INSERT INTO ci_runs(run_id,repo_id,mode,event,branch,workflow_path,created_at) VALUES(X'00000000000000000000000000000001',$1,'local','push','main','ci.yml',strftime('%Y-%m-%dT%H:%M:%f000Z','now','-120 seconds'))"
        };
        database.execute(insert, &[repo.id]).await;
        let queue = db::metrics::ci_queue(pool).await.unwrap();
        assert_eq!(queue.queued, 1);
        assert!(
            (119..=125).contains(&queue.oldest_seconds),
            "{}",
            queue.oldest_seconds
        );
        let root = tempfile::tempdir().unwrap();
        let mut state = state(&database, root.path()).await;
        state.metrics_token_hash = Some(token_hash("scrape-secret"));
        let router = create_router(state);
        let (status, _, body) = get(&router, "/metrics", Some("Bearer scrape-secret")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains(&format!(
                "codeyard_repository_bytes{{node_id=\"{}\"}} 512\n",
                primary.id
            )),
            "{body}"
        );
        assert!(
            body.contains(&format!(
                "codeyard_repository_bytes{{node_id=\"{}\"}} 512\n",
                replica.id
            )),
            "{body}"
        );
        assert!(body.contains(&format!(
            "codeyard_storage_free_bytes{{node_id=\"{}\"}} 8192\n",
            primary.id
        )));
        assert!(body.contains(&format!(
            "codeyard_storage_free_bytes{{node_id=\"{}\"}} NaN\n",
            replica.id
        )));
        assert!(body.contains("codeyard_replica_lag_versions 3\n"));
        assert!(body.contains("codeyard_ci_queued_runs 1\n"));
        for status in ["pending", "active", "revoked"] {
            assert!(
                body.contains(&format!("codeyard_nodes{{state=\"{status}\"}} 1\n")),
                "{body}"
            );
        }
        for secret in [
            "private-owner",
            "secret-repo",
            "/secret/path",
            "primary",
            "replica",
        ] {
            assert!(!body.contains(&format!("\"{secret}\"")));
        }
        database
            .execute(
                "UPDATE ci_runs SET status='running' WHERE repo_id=$1",
                &[repo.id],
            )
            .await;
        let queue = db::metrics::ci_queue(pool).await.unwrap();
        assert_eq!((queue.queued, queue.oldest_seconds), (0, 0));
        database
            .execute(
                "UPDATE repository_replicas SET applied_version=10 WHERE repository_id=$1",
                &[repo.id],
            )
            .await;
        assert_eq!(db::metrics::replica_lag(pool).await.unwrap(), 0);
        database.finish().await;
    }
}
