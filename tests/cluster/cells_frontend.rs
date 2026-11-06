//! Public HTTP selection against native cells and an authenticated directory.
use crate::support::native_database::PostgresDatabase;
use codeyard_cells_router::{directory, frontend, http::Transport, routes::Routes};
use codeyard_db_directory::Directory;
use codeyard_model::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use codeyard_support::{cells::Prefix, node_identity::NodeKey};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

pub(super) struct Server {
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
    pub(super) async fn start(app: axum::Router) -> Self {
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
    pub(super) async fn stop(&mut self) {
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(10), &mut self.task)
            .await
            .unwrap()
            .unwrap();
    }
    pub(super) fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}
pub(super) async fn native(database: &PostgresDatabase, name: &str) -> (Server, i64, String) {
    codeyard_db_cluster::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let user = codeyard_db_identity::users::accounts::create_user(&database.pool, name, None)
        .await
        .unwrap();
    let session = codeyard_identity::sessions::fresh_id(&database.pool).unwrap();
    codeyard_db_identity::users::sessions::create_session(
        &database.pool,
        &session,
        user.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let token = database
        .pool
        .cell_prefix()
        .token(codeyard_support::tokens::PAT_PREFIX, &"c".repeat(64))
        .unwrap();
    codeyard_db_identity::users::tokens::create_token(
        &database.pool,
        user.id,
        &session,
        "frontend",
        &codeyard_support::token_hash::token_hash(&token),
        "write",
        None,
    )
    .await
    .unwrap()
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
    (Server::start(app).await, user.id, token)
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
    let (mut alice, alice_id, alice_token) = native(&zero, "alice").await;
    let (mut bob, bob_id, bob_token) = native(&one, "bob").await;
    let directory_key = NodeKey::generate();
    let router_key = NodeKey::generate();
    let zero_key = NodeKey::generate();
    let one_key = NodeKey::generate();
    for (prefix, name, id, server, member) in [
        (Prefix::ZERO, "alice", alice_id, &alice, &zero_key),
        (Prefix::new(1), "bob", bob_id, &bob, &one_key),
    ] {
        directory
            .register_cell(Cell {
                id: prefix,
                name: format!("cell-{prefix}"),
                region: "a".into(),
                state: State::Open,
                web_urls: vec![server.url()],
                ssh_addresses: vec![],
                rpc_addresses: vec![],
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
    let mut homes = Vec::new();
    for (prefix, server, key, database) in [
        (Prefix::ZERO, &alice, &zero_key, &zero),
        (Prefix::new(1), &bob, &one_key, &one),
    ] {
        let client = directory::Client::new(
            key.clone(),
            listener.address,
            directory_key.public_key(),
            Default::default(),
        )
        .unwrap();
        let home = codeyard_cells_router::accounts::listen(
            client.clone(),
            Arc::new(codeyard_identity::cells::Local {
                pool: database.pool.clone(),
            }),
            key,
            "127.0.0.1:0".parse().unwrap(),
        )
        .await
        .unwrap();
        directory
            .register_cell(Cell {
                id: prefix,
                name: format!("cell-{prefix}"),
                region: "a".into(),
                state: State::Open,
                web_urls: vec![server.url()],
                ssh_addresses: vec![],
                rpc_addresses: vec![home.address.to_string()],
                node_keys: vec![key.public_hex()],
                database_bytes: 0,
                write_rate: 0,
                maximum_database_bytes: 1000,
                maximum_write_rate: 100,
            })
            .await
            .unwrap();
        database.pool.set_foreign_authorization(Arc::new(
            codeyard_cells_router::accounts::Remote::new(key.clone(), client).unwrap(),
        ));
        homes.push(home);
    }
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
    let encoded_client = reqwest::Client::builder()
        .no_gzip()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let compressed = encoded_client
        .post(format!("{}/api/graphql", bob.url()))
        .header("accept-encoding", "gzip")
        .bearer_auth(&bob_token)
        .json(&json!({"query":"{viewer{id login avatarUrl url}}"}))
        .send()
        .await
        .unwrap();
    assert_eq!(compressed.status(), 200);
    assert_eq!(compressed.headers()["content-encoding"], "gzip");
    let compressed = axum::body::to_bytes(
        axum::body::Body::from_stream(compressed.bytes_stream()),
        codeyard_cells_router::graphql_dispatch::Config::default().maximum_bytes,
    )
    .await
    .unwrap();
    assert!(compressed.starts_with(&[0x1f, 0x8b]));
    for (name, token) in [("alice", &alice_token), ("bob", &bob_token)] {
        let response = client
            .post(format!("{}/api/graphql", router.url()))
            .header("accept-encoding", "gzip")
            .header("authorization", format!("token {token}"))
            .json(&json!({"query":"query UserCurrent{viewer{login}}"}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert!(body.get("errors").is_none(), "{body}");
        assert_eq!(body["data"]["viewer"]["login"], name, "{body}");
    }
    for (name, id) in [("alice", alice_id), ("bob", bob_id)] {
        let response = client
            .get(format!("{}/api/v3/users/{name}", router.url()))
            .header("x-codeyard-user", "forged")
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["login"], name);
        assert_eq!(body["id"], id);
        let response = client
            .get(format!("{}/api/v3/user/{id}", router.url()))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, 200, "{body}");
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
    let alice_node = codeyard_model::node_id::encode("U", alice_id);
    let bob_node = codeyard_model::node_id::encode("U", bob_id);
    let query = json!({"query": r#"query($a:ID!,$b:ID!){left:repository(owner:"alice",name:"repo"){id name databaseId fullDatabaseId} right:repository(owner:"bob",name:"repo"){id name databaseId fullDatabaseId} a:node(id:$a){id ... on User{login databaseId}} b:node(id:$b){id ... on User{login databaseId}}}"#,
        "variables":{"a":alice_node,"b":bob_node}});
    let response = client
        .post(format!("{}/api/graphql", router.url()))
        .bearer_auth(&bob_token)
        .header("accept-encoding", "gzip")
        .json(&query)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    assert_eq!(body["data"]["left"]["name"], "repo");
    assert_eq!(body["data"]["right"]["name"], "repo");
    assert_ne!(body["data"]["left"]["id"], body["data"]["right"]["id"]);
    assert_eq!(body["data"]["a"]["login"], "alice");
    assert_eq!(body["data"]["b"]["login"], "bob");
    assert_eq!(body["data"]["a"]["databaseId"], alice_id);
    assert!(body["data"]["b"]["databaseId"].is_null());
    assert!(body["data"]["right"]["databaseId"].is_null());
    let full = body["data"]["right"]["fullDatabaseId"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    assert_eq!(Prefix::for_id(full).unwrap(), Prefix::new(1));
    assert_eq!(
        body["data"]["left"]["fullDatabaseId"],
        body["data"]["left"]["databaseId"].to_string()
    );

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
    for home in homes {
        home.stop().await;
    }
    transport.shutdown().await;
    listener.stop().await;
    directory.close().await;
    one.finish().await;
    zero.finish().await;
    registry.finish().await;
}
