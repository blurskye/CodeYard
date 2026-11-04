use crate::support::database::Database;
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use codeyard_support::{node_identity::NodeKey, token_hash::token_hash};
use tower::ServiceExt;

async fn node(db: &Database, name: &str, free: i64) -> i64 {
    let key = NodeKey::generate();
    let node = codeyard_db_cluster::cluster::register_own_node(
        db.pool(),
        &key.public_hex(),
        name,
        &["storage".into()],
        "test",
    )
    .await
    .unwrap();
    codeyard_db_cluster::cluster::record_node_heartbeat(
        db.pool(),
        &key.public_hex(),
        "test",
        "{}",
        Some(free),
    )
    .await
    .unwrap();
    codeyard_db_cluster::drain::record_capacity(db.pool(), &key.public_hex(), 10_000)
        .await
        .unwrap();
    node.id
}

async fn token(db: &Database, user: i64) -> String {
    let value = format!("cy_pat_{}", crate::support::native_database::token());
    let session = crate::support::native_database::token();
    codeyard_db_identity::users::sessions::create_session(
        db.pool(),
        &session,
        user,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    codeyard_db_identity::users::tokens::create_token(
        db.pool(),
        user,
        &session,
        "drain",
        &token_hash(&value),
        "write",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    value
}

async fn request(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn drain_capacity_and_authority_are_checked_through_rest_on_both_engines() {
    for pg in [false, true] {
        let mut db = Database::new(pg).await;
        db.complete_setup().await;
        let root = tempfile::tempdir().unwrap();
        let router = db.router(root.path()).await;
        let admin = db.actor("drain-admin").await;
        let credential = token(&db, admin).await;
        let source = node(&db, "source", 9000).await;
        let target = node(&db, "target", 1000).await;
        let repo = codeyard_db_repos::repositories::records::create_repository(
            db.pool(),
            admin,
            "placed",
            None,
            false,
            &format!("/repos/{}.git", uuid::Uuid::new_v4()),
        )
        .await
        .unwrap();
        codeyard_db_cluster::cluster::place_repository(db.pool(), repo.id, source)
            .await
            .unwrap();
        db.execute(
            "UPDATE repositories SET size_bytes=1000 WHERE id=$1",
            &[repo.id],
        )
        .await;
        let path = "/api/admin/nodes/source/drain";
        assert_eq!(
            request(&router, "POST", path, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&router, "POST", path, Some(&credential)).await.0,
            StatusCode::FORBIDDEN
        );
        db.execute("UPDATE users SET is_admin=true WHERE id=$1", &[admin])
            .await;
        let (status, body) = request(&router, "POST", path, Some(&credential)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body["missing_bytes"].as_i64().unwrap() > 0);
        assert_eq!(
            codeyard_db_cluster::cluster::get_node_by_id(db.pool(), source)
                .await
                .unwrap()
                .unwrap()
                .state,
            "active"
        );
        assert_eq!(
            db.scalar("SELECT COUNT(*) FROM node_drain_repositories", &[])
                .await,
            0
        );
        assert_eq!(
            db.scalar(
                "SELECT COUNT(*) FROM audit_log WHERE action='node.drain'",
                &[]
            )
            .await,
            0
        );
        db.execute("UPDATE nodes SET disk_free=9000 WHERE id=$1", &[target])
            .await;
        let (status, body) = request(&router, "POST", path, Some(&credential)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["state"], "draining");
        assert_eq!(body["drain_total"], 1);
        let nodes = codeyard_db_cluster::cluster::list_nodes(db.pool())
            .await
            .unwrap();
        assert!(
            !codeyard_db_core::models::ClusterNode::ranked_storage(&nodes)
                .iter()
                .any(|node| node.id == source)
        );
        assert!(
            !codeyard_db_cluster::drain::complete_repository(db.pool(), source, repo.id)
                .await
                .unwrap()
        );
        assert_eq!(
            request(&router, "DELETE", path, Some(&credential)).await.0,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            db.scalar("SELECT COUNT(*) FROM node_drain_repositories", &[])
                .await,
            0
        );
        db.execute("UPDATE users SET is_admin=false WHERE id=$1", &[admin])
            .await;
        assert_eq!(
            request(&router, "POST", path, Some(&credential)).await.0,
            StatusCode::FORBIDDEN
        );
        db.execute("UPDATE users SET is_admin=true WHERE id=$1", &[admin])
            .await;
        db.execute(
            "DELETE FROM personal_access_tokens WHERE user_id=$1",
            &[admin],
        )
        .await;
        assert_eq!(
            request(&router, "POST", path, Some(&credential)).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            codeyard_db_cluster::cluster::get_node_by_id(db.pool(), source)
                .await
                .unwrap()
                .unwrap()
                .state,
            "active"
        );
        db.finish().await;
    }
}
