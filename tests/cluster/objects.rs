//! Real authenticated storage sockets behind independent web routers, without S3.
use crate::support::{database::Database, http_client::Client};
use axum::http::StatusCode;
use codeyard_ci::config::CiConfig;
use codeyard_ci_engine::queue::RunQueue;
use codeyard_cluster_core::{
    pairing::PairingHandler,
    transport::{IrohDialer, NodeAddress, Relay},
};
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_repos_core::{
    context::NodeContext,
    wire::{Operation, Request},
};
use codeyard_storage::ObjectStore;
use codeyard_support::node_identity::NodeKey;
use serde_json::json;
use std::{path::Path, sync::Arc};

struct Node {
    context: NodeContext,
    listener: Option<codeyard_cluster::tcp::Listener>,
}
pub(super) async fn listen(context: &NodeContext) -> codeyard_cluster::tcp::Listener {
    let pool = context.database().unwrap();
    let queue = RunQueue::Database {
        pool: pool.clone(),
        config: Arc::new(CiConfig::default()),
        secrets: None,
        context: Some(context.clone()),
        notifications: Arc::new(NotificationBroadcaster::new(pool)),
    };
    let listener = codeyard_cluster::tcp::bind(
        "127.0.0.1:0".parse().unwrap(),
        &context.key,
        codeyard_cluster::tcp::Services {
            pool: pool.clone(),
            pairing: PairingHandler::new(pool.clone()),
            rpc: Some(codeyard_repos_rpc::RpcHandler::new(context.clone())),
            events: codeyard_cluster::ci::CiHandler::new(context.clone(), queue),
        },
    )
    .await
    .unwrap();
    codeyard_db_cluster::cluster::record_node_heartbeat(
        pool,
        &context.key.public_hex(),
        "test",
        &serde_json::to_string(&NodeAddress {
            tcp: vec![listener.addr()],
            ..Default::default()
        })
        .unwrap(),
        Some(9000),
    )
    .await
    .unwrap();
    codeyard_db_cluster::drain::record_capacity(pool, &context.key.public_hex(), 10_000)
        .await
        .unwrap();
    listener
}
async fn node(db: &Database, root: &Path, name: &str, role: &str) -> Node {
    let key = NodeKey::generate();
    let row = codeyard_db_cluster::cluster::register_own_node(
        db.pool(),
        &key.public_hex(),
        name,
        &[role.into()],
        "test",
    )
    .await
    .unwrap();
    let repos = root.join(name);
    std::fs::create_dir_all(&repos).unwrap();
    let context = NodeContext {
        pool: Some(db.pool().clone()),
        rpc: Arc::new(codeyard_repos_rpc::RpcFactory),
        iroh: IrohDialer::new(Relay::Off, key.clone()),
        key,
        node_id: row.id,
        repos_path: repos,
        peers: Default::default(),
        unreachable: Default::default(),
        clone_cursor: Default::default(),
        ci_replicas: Default::default(),
    };
    let listener = listen(&context).await;
    Node {
        context,
        listener: Some(listener),
    }
}
async fn router(db: &Database, root: &Path, context: &NodeContext) -> axum::Router {
    let storage = ObjectStore::on_nodes(
        root,
        codeyard_repos_core::objects::Store::new(context.clone()),
    )
    .await
    .unwrap();
    codeyard_api_rest::create_router(codeyard_app::app::AppState::new(
        db.pool().clone(),
        context.repos_path.clone(),
        Arc::new(storage),
        None,
        CiConfig::default(),
        Some(context.clone()),
    ))
}
async fn reserve(client: &Client, bytes: &[u8]) -> serde_json::Value {
    use sha2::Digest;
    client.expect("POST","/api/files",json!({"filename":"shared.txt","content_type":"text/plain","file_size":bytes.len(),"reason":"other","visibility":"owner","sha256_hex":hex::encode(sha2::Sha256::digest(bytes))}),StatusCode::OK).await
}
async fn put(client: &Client, reservation: &serde_json::Value, bytes: &[u8]) -> StatusCode {
    client
        .exchange(
            "PUT",
            reservation["upload_url"].as_str().unwrap(),
            "text/plain",
            bytes.to_vec(),
        )
        .await
        .status
}
async fn confirm(client: &Client, reservation: &serde_json::Value) {
    client
        .expect(
            "POST",
            "/api/files/confirm",
            json!({"file_id":reservation["file_id"]}),
            StatusCode::OK,
        )
        .await;
}
async fn download(client: &Client, reservation: &serde_json::Value) -> Vec<u8> {
    let response = client
        .exchange(
            "GET",
            reservation["upload_url"].as_str().unwrap(),
            "text/plain",
            Vec::new(),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    response.body
}

#[tokio::test]
async fn uploaded_files_survive_web_and_storage_loss_and_drain_without_s3_on_both_engines() {
    for pg in [false, true] {
        let mut db = Database::new(pg).await;
        db.complete_setup().await;
        crate::support::native_database::enable_cluster(db.pool())
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut nodes = Vec::new();
        for (name, role) in [
            ("web-a", "web"),
            ("web-b", "web"),
            ("store-a", "storage"),
            ("store-b", "storage"),
            ("store-c", "storage"),
            ("runner", "runner"),
        ] {
            nodes.push(node(&db, root.path(), name, role).await);
        }
        let a = router(&db, &root.path().join("cache-a"), &nodes[0].context).await;
        let b = router(&db, &root.path().join("cache-b"), &nodes[1].context).await;
        let user = db.actor("object-owner").await;
        let writer = Client::login(&a, db.pool(), user).await;
        let reader = Client {
            router: b,
            session: writer.session.clone(),
            csrf: writer.csrf.clone(),
        };
        let bytes = b"shared bytes acknowledged on two storage nodes";
        let upload = reserve(&writer, bytes).await;
        for free in [0, 1500, 10001] {
            db.execute(
                "UPDATE nodes SET disk_free=$1 WHERE id IN($2,$3,$4)",
                &[
                    free,
                    nodes[2].context.node_id,
                    nodes[3].context.node_id,
                    nodes[4].context.node_id,
                ],
            )
            .await;
            assert_eq!(
                put(&writer, &upload, bytes).await,
                StatusCode::SERVICE_UNAVAILABLE
            );
            assert_eq!(
                db.scalar("SELECT COUNT(*) FROM storage_objects", &[]).await,
                0
            );
        }
        db.execute(
            "UPDATE nodes SET disk_free=9000 WHERE id IN($1,$2,$3)",
            &[
                nodes[2].context.node_id,
                nodes[3].context.node_id,
                nodes[4].context.node_id,
            ],
        )
        .await;
        assert_eq!(put(&writer, &upload, bytes).await, StatusCode::NO_CONTENT);
        confirm(&writer, &upload).await;
        assert_eq!(
            db.scalar(
                "SELECT COUNT(*) FROM storage_object_copies WHERE ready=1",
                &[]
            )
            .await,
            2
        );
        assert_eq!(download(&reader, &upload).await, bytes);
        // No local cache or running uploader is needed on the second web node.
        drop(writer);
        nodes[2].listener.take().unwrap().stop().await;
        assert_eq!(download(&reader, &upload).await, bytes);
        let file = codeyard_db_repos::files::get_owned_file(
            db.pool(),
            upload["file_id"].as_str().unwrap().parse().unwrap(),
            user,
        )
        .await
        .unwrap()
        .unwrap();
        let (reply, _) = nodes[5]
            .context
            .call(
                nodes[3].context.node_id,
                Request {
                    repository: uuid::Uuid::nil(),
                    operation: Operation::ObjectGet {
                        key: file.object_key.clone(),
                        offset: 0,
                        length: None,
                    },
                    actor: None,
                    ci_run: None,
                },
                &[],
            )
            .await
            .unwrap();
        assert!(
            reply.result.is_err(),
            "runner cannot ask for arbitrary file bytes"
        );
        // The healthy first target writes, but an unavailable second target prevents acknowledgement.
        db.execute(
            "UPDATE nodes SET disk_free=9100 WHERE id=$1",
            &[nodes[3].context.node_id],
        )
        .await;
        db.execute(
            "UPDATE nodes SET disk_free=1500 WHERE id=$1",
            &[nodes[4].context.node_id],
        )
        .await;
        let pending = reserve(&reader, b"partial upload").await;
        assert_eq!(
            put(&reader, &pending, b"partial upload").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            db.scalar(
                "SELECT COUNT(*) FROM storage_objects WHERE confirmed=0",
                &[]
            )
            .await,
            1
        );
        assert_eq!(
            reader
                .exchange(
                    "POST",
                    "/api/files/confirm",
                    "application/json",
                    serde_json::to_vec(&json!({"file_id":pending["file_id"]})).unwrap()
                )
                .await
                .status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        nodes[2].listener = Some(listen(&nodes[2].context).await);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut retry = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                retry.tick().await;
                let status = put(&reader, &pending, b"partial upload").await;
                if status == StatusCode::NO_CONTENT {
                    break;
                }
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            }
        })
        .await
        .expect("storage retry should recover after the dial backoff");
        confirm(&reader, &pending).await;
        let hostile = reserve(&reader, b"declared").await;
        assert_eq!(
            put(&reader, &hostile, b"tampered").await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        db.execute(
            "UPDATE nodes SET disk_free=9000 WHERE id=$1",
            &[nodes[4].context.node_id],
        )
        .await;
        codeyard_cluster::drain::change(db.pool(), None, "store-a", false, None)
            .await
            .unwrap();
        assert!(
            !codeyard_db_cluster::drain::finish(db.pool(), nodes[2].context.node_id)
                .await
                .unwrap()
        );
        codeyard_repos_core::objects::maintain(&nodes[4].context)
            .await
            .unwrap();
        codeyard_repos_core::objects::maintain(&nodes[2].context)
            .await
            .unwrap();
        codeyard_cluster::drain::step(&nodes[2].context)
            .await
            .unwrap();
        assert_eq!(
            codeyard_db_cluster::cluster::get_node_by_id(db.pool(), nodes[2].context.node_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "drained"
        );
        assert_eq!(download(&reader, &upload).await, bytes);
        assert_eq!(download(&reader, &pending).await, b"partial upload");
        db.execute("UPDATE users SET is_banned=true WHERE id=$1", &[user])
            .await;
        assert_eq!(
            reader
                .exchange(
                    "GET",
                    upload["upload_url"].as_str().unwrap(),
                    "text/plain",
                    Vec::new()
                )
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        for node in nodes {
            if let Some(listener) = node.listener {
                listener.stop().await;
            }
            node.context.iroh.close().await;
        }
        db.finish().await;
    }
}
