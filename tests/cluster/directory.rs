//! Directory requests use real authenticated node-key TLS and live enrollment.
use crate::support::native_database::PostgresDatabase;
use codeyard_db_directory::Directory;
use codeyard_router::directory::{Client, Config, Listener, Reply, Request};
use codeyard_security::node_identity::NodeKey;
use codeyard_types::cell_ids::Prefix;
use codeyard_types::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn directory_transport_pins_keys_and_read_only_routers_cannot_claim_names() {
    let mut database = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&database.connection_url(), Default::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    let server = NodeKey::generate();
    let member = NodeKey::generate();
    let router = NodeKey::generate();
    let unknown = NodeKey::generate();
    directory
        .register_cell(Cell {
            id: Prefix::new(1),
            name: "first".into(),
            region: "a".into(),
            state: State::Open,
            web_urls: vec!["http://127.0.0.1:3000/".into()],
            ssh_addresses: vec![],
            rpc_addresses: vec![],
            node_keys: vec![member.public_hex()],
            database_bytes: 0,
            write_rate: 0,
            maximum_database_bytes: 1000,
            maximum_write_rate: 100,
            read_regions: Vec::new(),
        })
        .await
        .unwrap();
    directory
        .set_router_key(&router.public_hex(), true)
        .await
        .unwrap();
    let config = Config {
        exchange_seconds: 2,
        maximum_bytes: 4096,
        maximum_connections: 8,
    };
    let listener = Listener::bind(
        directory.clone(),
        &server,
        "127.0.0.1:0".parse().unwrap(),
        config,
    )
    .await
    .unwrap();
    let connect = |key| Client::new(key, listener.address, server.public_key(), config).unwrap();
    let member_client = connect(member.clone());
    let router_client = connect(router.clone());
    let cell = Request::Cell {
        prefix: Prefix::new(1),
    };
    assert!(matches!(
        router_client.request(&cell).await.unwrap(),
        Reply::Cell(Some(_))
    ));
    assert!(connect(unknown.clone()).request(&cell).await.is_err());
    let wrong_pin = Client::new(
        member.clone(),
        listener.address,
        unknown.public_key(),
        config,
    )
    .unwrap();
    assert!(wrong_pin.request(&cell).await.is_err());
    let next = Request::NextPrefix {
        previous: Prefix::new(1),
        last_id: *Prefix::new(1).ids().end(),
    };
    assert!(router_client.request(&next).await.is_err());
    assert!(
        matches!(member_client.request(&next).await.unwrap(), Reply::Prefix(prefix) if prefix==Prefix::new(2))
    );
    assert!(
        matches!(member_client.request(&next).await.unwrap(), Reply::Prefix(prefix) if prefix==Prefix::new(2))
    );
    let object = Object {
        kind: ObjectKind::User,
        id: *Prefix::new(1).ids().start(),
    };
    let key = Key::new(KeyKind::Name, "alice").unwrap();
    let reserve = Request::Reserve {
        object,
        keys: vec![key.clone()],
        nonce: "a".repeat(64),
    };
    assert!(
        router_client.request(&reserve).await.is_err(),
        "route readers have no cell mutation authority"
    );
    assert!(matches!(
        member_client.request(&reserve).await.unwrap(),
        Reply::Reserved
    ));
    assert!(matches!(
        router_client
            .request(&Request::Lookup { key: key.clone() })
            .await
            .unwrap(),
        Reply::Location(None)
    ));
    let confirm = Request::Confirm {
        object,
        keys: vec![key.clone()],
        nonce: "a".repeat(64),
    };
    // The caller authenticated already, but its next mutation must observe revocation.
    let mut revoked = directory.cell(Prefix::new(1)).await.unwrap().unwrap();
    revoked.node_keys = vec![unknown.public_hex()];
    directory.register_cell(revoked).await.unwrap();
    assert!(member_client.request(&confirm).await.is_err());
    assert!(member_client.request(&next).await.is_err());
    assert!(matches!(
        connect(unknown.clone()).request(&confirm).await.unwrap(),
        Reply::Done
    ));
    assert!(
        matches!(router_client.request(&Request::Lookup { key }).await.unwrap(), Reply::Location(Some(location)) if location.object==object)
    );
    directory
        .set_router_key(&router.public_hex(), false)
        .await
        .unwrap();
    assert!(router_client.request(&cell).await.is_err());
    // A hostile body is bounded on the server even when bypassing the client encoder.
    let mut link = codeyard_nodes::tcp::dial(
        &unknown,
        listener.address,
        server.public_key(),
        codeyard_router::directory::ALPN,
    )
    .await
    .unwrap();
    link.writer
        .write_all(&vec![b'x'; config.maximum_bytes + 1])
        .await
        .unwrap();
    link.finish().await.unwrap();
    let mut bytes = Vec::new();
    let closed = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        link.reader.read_to_end(&mut bytes),
    )
    .await
    .expect("oversized directory requests must close within the exchange deadline");
    assert!(
        closed.is_ok()
            || closed.is_err_and(|error| matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ))
    );

    assert!(bytes.is_empty());
    assert!(matches!(
        connect(unknown.clone()).request(&cell).await.unwrap(),
        Reply::Cell(Some(_))
    ));
    let email = Key::new(KeyKind::Email, "alice@example.test").unwrap();
    let reserve_email = Request::Reserve {
        object,
        keys: vec![email.clone()],
        nonce: "b".repeat(64),
    };
    let confirm_email = Request::Confirm {
        object,
        keys: vec![email.clone()],
        nonce: "b".repeat(64),
    };
    connect(unknown.clone())
        .request(&reserve_email)
        .await
        .unwrap();
    connect(unknown.clone())
        .request(&confirm_email)
        .await
        .unwrap();
    directory
        .set_router_key(&router.public_hex(), true)
        .await
        .unwrap();
    let release = Request::Release {
        object,
        keys: vec![email.clone()],
    };
    assert!(router_client.request(&release).await.is_err());
    assert!(
        member_client.request(&release).await.is_err(),
        "revoked cell key cannot remove claims"
    );
    connect(unknown.clone()).request(&release).await.unwrap();
    connect(unknown.clone()).request(&release).await.unwrap();
    let replacement = Object {
        kind: ObjectKind::User,
        id: object.id + 1,
    };
    let replacement_keys = vec![
        Key::new(KeyKind::Name, "replacement").unwrap(),
        email.clone(),
    ];
    connect(unknown.clone())
        .request(&Request::Reserve {
            object: replacement,
            keys: replacement_keys.clone(),
            nonce: "c".repeat(64),
        })
        .await
        .unwrap();
    connect(unknown.clone())
        .request(&Request::Confirm {
            object: replacement,
            keys: replacement_keys,
            nonce: "c".repeat(64),
        })
        .await
        .unwrap();
    connect(unknown.clone()).request(&release).await.unwrap();
    assert_eq!(
        directory.lookup(&email).await.unwrap().unwrap().object,
        replacement,
        "late cleanup cannot remove a replacement claim"
    );
    let stranger = NodeKey::generate();
    directory
        .register_cell(Cell {
            id: Prefix::new(3),
            name: "other".into(),
            region: "a".into(),
            state: State::Open,
            web_urls: vec!["http://127.0.0.1:3001".into()],
            ssh_addresses: vec![],
            rpc_addresses: vec![],
            node_keys: vec![stranger.public_hex()],
            database_bytes: 0,
            write_rate: 0,
            maximum_database_bytes: 1000,
            maximum_write_rate: 100,
            read_regions: Vec::new(),
        })
        .await
        .unwrap();
    let forget = Request::Forget {
        object: replacement,
    };
    assert!(connect(stranger).request(&forget).await.is_err());
    assert!(member_client.request(&forget).await.is_err());
    connect(unknown.clone()).request(&forget).await.unwrap();
    connect(unknown.clone()).request(&forget).await.unwrap();
    assert_eq!(directory.lookup(&email).await.unwrap(), None);
    let routes =
        codeyard_router::routes::Routes::new(connect(unknown.clone()), Default::default()).unwrap();
    assert_eq!(
        routes.by_name("alice").await.unwrap().unwrap().id,
        Prefix::new(1)
    );
    listener.stop().await;
    assert_eq!(
        routes.by_name("ALICE").await.unwrap().unwrap().id,
        Prefix::new(1),
        "a cached route survives a directory outage within its TTL"
    );
    assert!(routes.by_name("uncached").await.is_err());
    directory.close().await;
    database.finish().await;
}
