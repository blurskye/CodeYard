//! `/issues` and `/pulls` list the signed-in user's own work, so a visitor is sent to sign in.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use codeyard::backend::{api::create_router, app::AppState, ci::CiConfig, storage::ObjectStore};
use database::Database;
use std::sync::Arc;
use tower::ServiceExt;

#[tokio::test]
async fn signed_out_visitors_are_sent_to_sign_in_from_the_work_lists() {
    let mut f = Database::new(false).await;
    f.execute(
        "INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE",
        &[],
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    for path in ["/issues", "/pulls"] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{path}");
        assert_eq!(response.headers()["location"], "/login", "{path}");
    }
    f.finish().await;
}
