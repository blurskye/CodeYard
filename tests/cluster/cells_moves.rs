//! A token keeps working after its user moves to another cell: the router sends account-wide
//! requests to the new home, and the cell that issued the token asks the new home about it.
use super::cells_frontend::{native, Server};
use crate::support::native_database::PostgresDatabase;
use codeyard_cells_router::{directory, frontend, http::Transport, routes::Routes};
use codeyard_db_cluster::moves::{self, Account};
use codeyard_db_directory::Directory;
use codeyard_model::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use codeyard_support::{cells::Prefix, node_identity::NodeKey};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

fn cell(prefix: Prefix, web: &Server, rpc: Option<String>, key: &NodeKey) -> Cell {
    Cell {
        id: prefix,
        name: format!("cell-{prefix}"),
        region: "a".into(),
        state: State::Open,
        web_urls: vec![web.url()],
        ssh_addresses: vec![],
        rpc_addresses: rpc.into_iter().collect(),
        node_keys: vec![key.public_hex()],
        database_bytes: 0,
        write_rate: 0,
        maximum_database_bytes: 1000,
        maximum_write_rate: 100,
        read_regions: Vec::new(),
    }
}

#[tokio::test]
async fn a_moved_users_token_follows_the_user_to_the_new_home() {
    let mut registry = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&registry.connection_url(), Default::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    let mut source = PostgresDatabase::new().await;
    let mut target = PostgresDatabase::new().await;
    for (database, prefix) in [(&source, 1), (&target, 2)] {
        codeyard_db_core::cells::initialize(&database.pool, Prefix::new(prefix))
            .await
            .unwrap();
        codeyard_db_core::cells::load(&database.pool).await.unwrap();
    }
    let (mut source_server, alice, alice_token) = native(&source, "alice").await;
    let (mut target_server, bob, bob_token) = native(&target, "bob").await;
    assert!(alice_token.starts_with("cy_pat_1_") && bob_token.starts_with("cy_pat_2_"));
    // Carol and her public board stay in the source cell.
    let carol = codeyard_db_identity::users::accounts::create_user(&source.pool, "carol", None)
        .await
        .unwrap();
    let board = tempfile::tempdir().unwrap();
    git2::Repository::init_bare(board.path().join("board.git")).unwrap();
    codeyard_db_repos::repositories::records::create_repository(
        &source.pool,
        carol.id,
        "board",
        None,
        false,
        board.path().join("board.git").to_str().unwrap(),
    )
    .await
    .unwrap();
    let directory_key = NodeKey::generate();
    let router_key = NodeKey::generate();
    let source_key = NodeKey::generate();
    let target_key = NodeKey::generate();
    for (prefix, server, key, names) in [
        (
            Prefix::new(1),
            &source_server,
            &source_key,
            vec![("alice", alice), ("carol", carol.id)],
        ),
        (
            Prefix::new(2),
            &target_server,
            &target_key,
            vec![("bob", bob)],
        ),
    ] {
        directory
            .register_cell(cell(prefix, server, None, key))
            .await
            .unwrap();
        for (name, id) in names {
            let object = Object {
                kind: ObjectKind::User,
                id,
            };
            let keys = [Key::new(KeyKind::Name, name).unwrap()];
            let nonce = "b".repeat(64);
            directory
                .reserve(&key.public_hex(), object, &keys, &nonce)
                .await
                .unwrap();
            directory
                .confirm(&key.public_hex(), object, &keys, &nonce)
                .await
                .unwrap();
        }
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
        (Prefix::new(1), &source_server, &source_key, &source),
        (Prefix::new(2), &target_server, &target_key, &target),
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
            .register_cell(cell(prefix, server, Some(home.address.to_string()), key))
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

    // Move alice and her repository to the target cell.
    let account = Account::new(ObjectKind::User, alice).unwrap();
    let wait = Duration::from_secs(30);
    moves::begin(&source.raw, account, 2, wait).await.unwrap();
    // This fixture has no peer outbox workers; acknowledge the source's notices.
    sqlx::query("UPDATE cell_outbox SET delivered_at=clock_timestamp() WHERE delivered_at IS NULL")
        .execute(&source.raw)
        .await
        .unwrap();
    moves::freeze(&source.raw, account, Duration::from_secs(300), wait)
        .await
        .unwrap();
    let paths: BTreeMap<i64, String> = sqlx::query_as::<_, (i64, String)>(
        "SELECT id,disk_path FROM repositories WHERE owner_user_id=$1",
    )
    .bind(alice)
    .fetch_all(&source.raw)
    .await
    .unwrap()
    .into_iter()
    .collect();
    assert_eq!(paths.len(), 1);
    moves::copy(&source.raw, &target.raw, account, 2, &paths, None)
        .await
        .unwrap();
    let credentials = moves::credentials(&source.raw, account).await.unwrap();
    assert_eq!(credentials.len(), 1);
    let object = Object {
        kind: ObjectKind::User,
        id: alice,
    };
    // Publishing twice, as a repeated move run does, keeps one key.
    for nonce in ["c".repeat(64), "d".repeat(64)] {
        directory
            .reserve(&source_key.public_hex(), object, &credentials, &nonce)
            .await
            .unwrap();
        directory
            .confirm(&source_key.public_hex(), object, &credentials, &nonce)
            .await
            .unwrap();
    }
    moves::switch(&source.raw, account).await.unwrap();
    moves::activate(&source.raw, &target.raw, account)
        .await
        .unwrap();
    directory
        .move_home(&source_key.public_hex(), object, Prefix::new(2))
        .await
        .unwrap();
    moves::release(&source.raw, account, None).await.unwrap();
    moves::finish(&source.raw, account).await.unwrap();
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM personal_access_tokens WHERE user_id=$1")
            .bind(alice)
            .fetch_one(&source.raw)
            .await
            .unwrap();
    assert_eq!(left, 0, "the source keeps no copy of a moved token");
    let location = directory.lookup(&credentials[0]).await.unwrap().unwrap();
    assert_eq!((location.cell, location.object), (Prefix::new(2), object));

    // Account-wide REST and GraphQL requests reach the new home.
    let response = client
        .get(format!("{}/api/v3/user", router.url()))
        .bearer_auth(&alice_token)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (&body["login"], &body["id"]),
        (&json!("alice"), &json!(alice))
    );
    let response = client
        .post(format!("{}/api/graphql", router.url()))
        .bearer_auth(&alice_token)
        .json(&json!({"query":"{viewer{login}}"}))
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["data"]["viewer"]["login"], "alice", "{body}");
    // Git over HTTP at the new home accepts the token issued by the old one.
    let response = client
        .get(format!(
            "{}/alice/repo.git/info/refs?service=git-receive-pack",
            router.url()
        ))
        .basic_auth("alice", Some(&alice_token))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    // A repository left in the old cell asks the new home about the token.
    let response = client
        .post(format!("{}/api/v3/repos/carol/board/issues", router.url()))
        .bearer_auth(&alice_token)
        .json(&json!({"title":"From the new home"}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["user"]["login"], "alice", "{body}");
    // Unknown tokens with either prefix are refused, not reported as unavailable.
    for forged in [
        format!("cy_pat_1_{}", "a".repeat(64)),
        format!("cy_pat_2_{}", "a".repeat(64)),
    ] {
        for path in ["/api/v3/user", "/api/v3/repos/carol/board/issues"] {
            let response = client
                .get(format!("{}{path}", router.url()))
                .bearer_auth(&forged)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 401, "{path}");
        }
    }
    let response = client
        .get(format!("{}/api/v3/user", router.url()))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["login"], "bob", "{body}");

    // Revoking the token at the new home stops it everywhere: each request asks the home.
    sqlx::query("DELETE FROM personal_access_tokens WHERE user_id=$1")
        .bind(alice)
        .execute(&target.raw)
        .await
        .unwrap();
    let response = client
        .get(format!("{}/api/v3/user", router.url()))
        .bearer_auth(&alice_token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let response = client
        .post(format!("{}/api/v3/repos/carol/board/issues", router.url()))
        .bearer_auth(&alice_token)
        .json(&json!({"title":"Must not land"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    drop(client);
    router.stop().await;
    source_server.stop().await;
    target_server.stop().await;
    for home in homes {
        home.stop().await;
    }
    transport.shutdown().await;
    listener.stop().await;
    directory.close().await;
    target.finish().await;
    source.finish().await;
    registry.finish().await;
}
