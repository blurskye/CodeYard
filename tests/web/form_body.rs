//! Completed form buffers remain admitted while their handlers wait for the database.
use crate::support::database::Database;
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db::DbPool, storage::ObjectStore,
};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

#[tokio::test]
async fn completed_forms_keep_their_slots_until_handler_completion() {
    let mut database = Database::new(false).await;
    let root = tempfile::tempdir().unwrap();
    let state = AppState::new(
        database.pool().clone(),
        root.path().into(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let router = create_router(state);
    let DbPool::Sqlite(pool) = database.pool() else {
        panic!("not SQLite")
    };
    let mut connections = Vec::new();
    for _ in 0..pool.options().get_max_connections() {
        connections.push(pool.acquire().await.unwrap());
    }
    let completed = Arc::new(tokio::sync::Semaphore::new(0));
    let request = || {
        let bytes = bytes::Bytes::from(format!("title=draft&body={}", "x".repeat(1024 * 1024)));
        let completed = completed.clone();
        let body = Body::from_stream(futures_util::stream::unfold(
            (Some(bytes), completed),
            |(bytes, completed)| async move {
                match bytes {
                    Some(bytes) => Some((Ok::<_, std::io::Error>(bytes), (None, completed))),
                    None => {
                        completed.add_permits(1);
                        None
                    }
                }
            },
        ));
        Request::builder()
            .method("POST")
            .uri("/alice/proj/issues/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .unwrap()
    };
    let mut tasks = Vec::new();
    for _ in 0..8 {
        tasks.push(tokio::spawn(router.clone().oneshot(request())));
    }
    tokio::time::timeout(Duration::from_secs(5), completed.acquire_many(8))
        .await
        .unwrap()
        .unwrap()
        .forget();
    let refused = tokio::time::timeout(Duration::from_secs(2), router.oneshot(request()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    for task in tasks {
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    drop(connections);
    database.finish().await;
}
