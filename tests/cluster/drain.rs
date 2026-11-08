use crate::support::database::Database;
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use codeyard_support::{node_identity::NodeKey, token_hash::token_hash};
use tower::ServiceExt;

pub(super) async fn node(db: &Database, name: &str, free: i64) -> i64 {
    let key = NodeKey::generate();
    let node = codeyard_db_placement::cluster::register_own_node(
        db.pool(),
        &key.public_hex(),
        name,
        &["storage".into()],
        "test",
    )
    .await
    .unwrap();
    codeyard_db_placement::cluster::record_node_heartbeat(
        db.pool(),
        &key.public_hex(),
        "test",
        "{}",
        Some(free),
    )
    .await
    .unwrap();
    codeyard_db_placement::drain::record_capacity(db.pool(), &key.public_hex(), 10_000)
        .await
        .unwrap();
    node.id
}

pub(super) async fn token(db: &Database, user: i64) -> String {
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
        codeyard_db_placement::cluster::place_repository(db.pool(), repo.id, source)
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
            codeyard_db_placement::cluster::get_node_by_id(db.pool(), source)
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
        let nodes = codeyard_db_placement::cluster::list_nodes(db.pool())
            .await
            .unwrap();
        assert!(
            !codeyard_database::models::ClusterNode::ranked_storage(&nodes)
                .iter()
                .any(|node| node.id == source)
        );
        assert!(
            !codeyard_db_placement::drain::complete_repository(db.pool(), source, repo.id)
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
            codeyard_db_placement::cluster::get_node_by_id(db.pool(), source)
                .await
                .unwrap()
                .unwrap()
                .state,
            "active"
        );
        db.finish().await;
    }
}

#[tokio::test]
async fn provisioning_api_limits_invitation_uses_and_rejects_request_commands() {
    use codeyard_git_storage::context::NodeContext;
    use codeyard_nodes::{
        pairing::JoinTicket,
        transport::{IrohDialer, NodeAddress, Relay},
    };
    use std::sync::Arc;
    for pg in [false, true] {
        let mut db = Database::new(pg).await;
        db.complete_setup().await;
        let root = tempfile::tempdir().unwrap();
        let admin = db.actor("provision-admin").await;
        db.execute("UPDATE users SET is_admin=true WHERE id=$1", &[admin])
            .await;
        let credential = token(&db, admin).await;
        let key = NodeKey::generate();
        let node = codeyard_db_placement::cluster::register_own_node(
            db.pool(),
            &key.public_hex(),
            "core",
            &["web".into()],
            "test",
        )
        .await
        .unwrap();
        let address = NodeAddress {
            tcp: vec!["127.0.0.1:9".parse().unwrap()],
            ..Default::default()
        };
        codeyard_db_placement::cluster::record_node_heartbeat(
            db.pool(),
            &key.public_hex(),
            "test",
            &serde_json::to_string(&address).unwrap(),
            None,
        )
        .await
        .unwrap();
        let context = NodeContext {
            pool: Some(db.pool().clone()),
            rpc: Arc::new(codeyard_repos_rpc::RpcFactory),
            iroh: IrohDialer::new(Relay::Off, key.clone()),
            key,
            node_id: node.id,
            repos_path: root.path().to_owned(),
            peers: Default::default(),
            unreachable: Default::default(),
            clone_cursor: Default::default(),
            ci_replicas: Default::default(),
        };
        let state = codeyard_app::app::AppState::new(
            db.pool().clone(),
            root.path().to_owned(),
            Arc::new(
                codeyard_storage::ObjectStore::open(root.path(), None)
                    .await
                    .unwrap(),
            ),
            None,
            codeyard_ci::config::CiConfig::default(),
            Some(context.clone()),
        );
        let router = codeyard_api_rest::create_router(state);
        let add = |body: &str| {
            Request::builder()
                .method("POST")
                .uri("/api/admin/nodes")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {credential}"))
                .body(Body::from(body.to_owned()))
                .unwrap()
        };
        let response = router
            .clone()
            .oneshot(add(r#"{"role":"runner","count":2,"command":"false"}"#))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(db.scalar("SELECT COUNT(*) FROM join_tokens", &[]).await, 0);
        let response = router
            .clone()
            .oneshot(add(r#"{"role":"runner","count":2}"#))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        let ticket = JoinTicket::decode(body["ticket"].as_str().unwrap()).unwrap();
        for index in 0..3 {
            let key = NodeKey::generate();
            let outcome = codeyard_db_placement::cluster::join_with_token(
                db.pool(),
                codeyard_db_placement::cluster::JoinRequest {
                    token_hash: &ticket.secret_hash(),
                    public_key: &key.public_hex(),
                    name: &format!("joined-{index}"),
                    roles: Some(&["runner".into()]),
                    version: "test",
                },
            )
            .await
            .unwrap();
            if index < 2 {
                assert!(matches!(
                    outcome,
                    codeyard_database::models::JoinOutcome::Joined(_)
                ));
            } else {
                assert!(matches!(
                    outcome,
                    codeyard_database::models::JoinOutcome::InvalidToken
                ));
            }
        }
        db.execute("UPDATE users SET is_admin=false WHERE id=$1", &[admin])
            .await;
        assert_eq!(
            router
                .oneshot(add(r#"{"role":"runner","count":2}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        context.iroh.close().await;
        db.finish().await;
    }
}
