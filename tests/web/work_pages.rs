//! `/issues` and `/pulls` list the signed-in user's own work, so a visitor is sent to sign in.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use database::Database;
use tower::ServiceExt;

#[tokio::test]
async fn signed_out_visitors_are_sent_to_sign_in_from_the_work_lists() {
    let mut f = Database::new(false).await;
    f.complete_setup().await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
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
