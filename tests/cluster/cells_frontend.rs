//! Public HTTP selection against native cells and an authenticated directory.
use crate::support::native_database::PostgresDatabase;
use codeyard_cells_router::{directory, frontend, http::Transport, routes::Routes};
use codeyard_db_directory::Directory;
use codeyard_model::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use codeyard_support::{cells::Prefix, node_identity::NodeKey};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

struct Server {
    address: SocketAddr,
    stop: CancellationToken,
    task: AbortOnDropHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
impl Server {
    async fn start(app: axum::Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(stopping.cancelled_owned())
            .await
            .unwrap();
        }));
        Self {
            address,
            stop,
            task,
        }
    }
    async fn stop(&mut self) {
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(10), &mut self.task)
            .await
            .unwrap()
            .unwrap();
    }
    fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}
async fn native(database: &PostgresDatabase, name: &str) -> (Server, i64) {
    let user = codeyard_db_identity::users::accounts::create_user(&database.pool, name, None)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo.git");
    git2::Repository::init_bare(&repo).unwrap();
    codeyard_db_repos::repositories::records::create_repository(
        &database.pool,
        user.id,
        "repo",
        None,
        false,
        repo.to_str().unwrap(),
    )
    .await
    .unwrap();
    let storage = Arc::new(
        codeyard_storage::ObjectStore::open(root.path(), None)
            .await
            .unwrap(),
    );
    let app = codeyard_api_rest::create_router(codeyard_app::app::AppState::new(
        database.pool.clone(),
        root.path().into(),
        storage,
        None,
        Default::default(),
        None,
    ));
    // The server task owns the temporary Git directory until its listener drains.
    let app = app.layer(axum::Extension(Arc::new(root)));
    (Server::start(app).await, user.id)
}
#[tokio::test]
async fn native_http_cells_route_by_owner_and_keep_working_when_another_cell_stops() {
    let mut registry = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&registry.connection_url(), Default::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    let mut zero = PostgresDatabase::new().await;
    let mut one = PostgresDatabase::new().await;
    codeyard_db_core::cells::initialize(&one.pool, Prefix::new(1))
        .await
        .unwrap();
    let (mut alice, alice_id) = native(&zero, "alice").await;
    let (mut bob, bob_id) = native(&one, "bob").await;
    let directory_key = NodeKey::generate();
    let router_key = NodeKey::generate();
    for (prefix, name, id, server) in [
        (Prefix::ZERO, "alice", alice_id, &alice),
        (Prefix::new(1), "bob", bob_id, &bob),
    ] {
        let member = NodeKey::generate();
        directory
            .register_cell(Cell {
                id: prefix,
                name: format!("cell-{prefix}"),
                region: "a".into(),
                state: State::Open,
                web_urls: vec![server.url()],
                ssh_addresses: vec![],
                node_keys: vec![member.public_hex()],
                database_bytes: 0,
                write_rate: 0,
                maximum_database_bytes: 1000,
                maximum_write_rate: 100,
            })
            .await
            .unwrap();
        let object = Object {
            kind: ObjectKind::User,
            id,
        };
        let keys = [Key::new(KeyKind::Name, name).unwrap()];
        let nonce = "b".repeat(64);
        directory
            .reserve(&member.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
        directory
            .confirm(&member.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
    }
    directory
        .set_router_key(&router_key.public_hex(), true)
        .await
        .unwrap();
    let listener = directory::Listener::bind(
        directory.clone(),
        &directory_key,
        "127.0.0.1:0".parse().unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    let routes = Routes::new(
        directory::Client::new(
            router_key.clone(),
            listener.address,
            directory_key.public_key(),
            Default::default(),
        )
        .unwrap(),
        Default::default(),
    )
    .unwrap();
    let transport = Transport::new(Default::default()).unwrap();
    let frontend = frontend::Frontend::new(
        routes,
        transport.clone(),
        codeyard_api_rest::routing_catalog().unwrap(),
        frontend::Config {
            region: "a".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let mut router = Server::start(frontend.router()).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    for (name, id) in [("alice", alice_id), ("bob", bob_id)] {
        let response = client
            .get(format!("{}/api/v3/users/{name}", router.url()))
            .header("x-codeyard-user", "forged")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["login"], name);
        assert_eq!(body["id"], id);
        let repo = client
            .get(format!("{}/api/v3/repos/{name}/repo", router.url()))
            .send()
            .await
            .unwrap();
        assert_eq!(repo.status(), 200);
    }
    let response = client
        .post(format!("{}/api/graphql", router.url()))
        .json(&json!({"query":"{viewer{id}}"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    // No idle connection can keep a stopped cell reachable.
    bob.stop().await;
    assert_eq!(
        client
            .get(format!("{}/api/v3/users/alice", router.url()))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let response = client
        .get(format!("{}/api/v3/users/bob", router.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert!(response.json::<Value>().await.unwrap()["message"]
        .as_str()
        .unwrap()
        .contains("cell is unavailable"));
    let response = client
        .get(format!("{}/api/v3/users/alice", router.url()))
        .bearer_auth("hostile")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    directory
        .set_router_key(&router_key.public_hex(), false)
        .await
        .unwrap();
    // Cached owner routes remain hints only; native authentication still rejects forged credentials.
    let response = client
        .get(format!("{}/api/v3/user", router.url()))
        .bearer_auth(format!("cy_pat_0_{}", "a".repeat(64)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    drop(client);
    router.stop().await;
    alice.stop().await;
    transport.shutdown().await;
    listener.stop().await;
    directory.close().await;
    one.finish().await;
    zero.finish().await;
    registry.finish().await;
}
