//! Regions through the real router: signups land in the router's region, accounts keep their
//! home, an organization with presence reads from its home cell's web nodes in that region and
//! writes at home, and a cell moved to its paired region is served there.
use super::cells_frontend::Server;
use crate::support::native_database::PostgresDatabase;
use codeyard_cells_router::{
    directory, frontend,
    http::Transport,
    routes::{self, Routes},
};
use codeyard_db_directory::Directory;
use codeyard_model::cells::{Cell, Key, KeyKind, Object, ObjectKind, ReadRegion, State};
use codeyard_support::{cells::Prefix, node_identity::NodeKey};
use std::time::Duration;

/// A stand-in cell web node that names itself in every response.
async fn web_node(name: &'static str) -> Server {
    Server::start(axum::Router::new().fallback(
        move |method: axum::http::Method, uri: axum::http::Uri| async move {
            format!("{name} {method} {}", uri.path())
        },
    ))
    .await
}

fn record(
    prefix: Prefix,
    region: &str,
    web_url: String,
    key: &NodeKey,
    read_regions: Vec<ReadRegion>,
) -> Cell {
    Cell {
        id: prefix,
        name: format!("cell-{prefix}"),
        region: region.into(),
        state: State::Open,
        web_urls: vec![web_url],
        ssh_addresses: vec![],
        rpc_addresses: vec![],
        node_keys: vec![key.public_hex()],
        database_bytes: 0,
        write_rate: 0,
        maximum_database_bytes: 1000,
        maximum_write_rate: 100,
        read_regions,
    }
}

fn router(
    region: &str,
    key: &NodeKey,
    listener: &directory::Listener,
    directory_key: &NodeKey,
) -> axum::Router {
    let routes = Routes::new(
        directory::Client::new(
            key.clone(),
            listener.address,
            directory_key.public_key(),
            Default::default(),
        )
        .unwrap(),
        routes::Config {
            cache_seconds: 1,
            ..Default::default()
        },
    )
    .unwrap();
    frontend::Frontend::new(
        routes,
        Transport::new(Default::default()).unwrap(),
        codeyard_api_rest::routing_catalog().unwrap(),
        frontend::Config {
            region: region.into(),
            ..Default::default()
        },
    )
    .unwrap()
    .router()
}

/// Which web node answered, as "<node> <method> <path>".
async fn answered(request: reqwest::RequestBuilder) -> String {
    let response = request.send().await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    body
}

async fn expire_routes() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

#[tokio::test]
async fn regions_place_signups_keep_homes_and_serve_present_organizations_nearby() {
    let registry = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&registry.connection_url(), Default::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    let zero = web_node("zero").await;
    let mut zero_b = web_node("zero-b").await;
    let one = web_node("one").await;
    let [zero_key, one_key, router_key, directory_key] = [(); 4].map(|()| NodeKey::generate());
    let nearby = vec![ReadRegion {
        region: "b".into(),
        web_urls: vec![zero_b.url()],
    }];
    // A read region must be another region, listed once.
    for hostile in [
        vec![ReadRegion {
            region: "a".into(),
            web_urls: vec![zero_b.url()],
        }],
        [nearby.clone(), nearby.clone()].concat(),
        vec![ReadRegion {
            region: "b".into(),
            web_urls: vec!["http://example.test/".into()],
        }],
    ] {
        assert!(directory
            .register_cell(record(Prefix::ZERO, "a", zero.url(), &zero_key, hostile))
            .await
            .is_err());
    }
    directory
        .register_cell(record(
            Prefix::ZERO,
            "a",
            zero.url(),
            &zero_key,
            nearby.clone(),
        ))
        .await
        .unwrap();
    directory
        .register_cell(record(Prefix::new(1), "b", one.url(), &one_key, vec![]))
        .await
        .unwrap();
    let alice = Object {
        kind: ObjectKind::User,
        id: 1,
    };
    let acme = Object {
        kind: ObjectKind::Organization,
        id: 2,
    };
    for (object, name, nonce) in [(alice, "alice", "b"), (acme, "acme", "c")] {
        let keys = [Key::new(KeyKind::Name, name).unwrap()];
        let nonce = nonce.repeat(64);
        directory
            .reserve(&zero_key.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
        directory
            .confirm(&zero_key.public_hex(), object, &keys, &nonce)
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
    let a = Server::start(router("a", &router_key, &listener, &directory_key)).await;
    let b = Server::start(router("b", &router_key, &listener, &directory_key)).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let get = |router: &Server, path: &str| client.get(format!("{}{path}", router.url()));
    let post = |router: &Server, path: &str| client.post(format!("{}{path}", router.url()));

    // Signups go to an open cell in the router's own region.
    let signup = || "mode=signup&username=carol";
    assert_eq!(
        answered(
            post(&b, "/enter")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(signup())
        )
        .await,
        "one POST /enter"
    );
    assert_eq!(
        answered(
            post(&a, "/enter")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(signup())
        )
        .await,
        "zero POST /enter"
    );
    // An account keeps its home cell whichever region it works from.
    assert_eq!(
        answered(get(&b, "/api/v3/users/alice")).await,
        "zero GET /api/v3/users/alice"
    );
    assert_eq!(
        answered(get(&b, "/alice/tools")).await,
        "zero GET /alice/tools"
    );
    // Without presence, an organization's reads from B go home.
    assert_eq!(
        answered(get(&b, "/acme/tools")).await,
        "zero GET /acme/tools"
    );
    assert!(directory.set_presence(999, "b", true).await.is_err());
    assert!(directory.set_presence(alice.id, "b", true).await.is_err());
    assert!(directory.set_presence(acme.id, "B!", true).await.is_err());
    assert!(directory.set_presence(acme.id, "b", true).await.unwrap());
    assert!(!directory.set_presence(acme.id, "b", true).await.unwrap());
    assert_eq!(directory.presence(acme.id).await.unwrap(), ["b"]);
    expire_routes().await;

    // With presence, reads from B use B's web nodes; writes and reads from A go home.
    for path in [
        "/acme/tools",
        "/api/v3/repos/acme/tools/issues",
        "/acme/tools.git/info/refs",
    ] {
        assert_eq!(answered(get(&b, path)).await, format!("zero-b GET {path}"));
        assert_eq!(answered(get(&a, path)).await, format!("zero GET {path}"));
    }
    assert_eq!(
        answered(post(&b, "/acme/tools.git/git-upload-pack").body("0000")).await,
        "zero-b POST /acme/tools.git/git-upload-pack"
    );
    assert_eq!(
        answered(post(&b, "/acme/tools.git/git-receive-pack").body("0000")).await,
        "zero POST /acme/tools.git/git-receive-pack"
    );
    assert_eq!(
        answered(
            post(&b, "/api/v3/repos/acme/tools/issues").json(&serde_json::json!({"title": "x"}))
        )
        .await,
        "zero POST /api/v3/repos/acme/tools/issues"
    );
    // A GET that carries a body, or a fetch larger than the router buffers, goes home.
    assert_eq!(
        answered(get(&b, "/acme/tools").body("x")).await,
        "zero GET /acme/tools"
    );
    let large = vec![b'0'; frontend::Config::default().maximum_control_bytes + 1];
    assert_eq!(
        answered(post(&b, "/acme/tools.git/git-upload-pack").body(large)).await,
        "zero POST /acme/tools.git/git-upload-pack"
    );

    // Removing presence sends B's reads home again.
    assert!(directory.set_presence(acme.id, "b", false).await.unwrap());
    expire_routes().await;
    assert_eq!(
        answered(get(&b, "/acme/tools")).await,
        "zero GET /acme/tools"
    );
    assert!(directory.set_presence(acme.id, "b", true).await.unwrap());
    expire_routes().await;
    assert_eq!(
        answered(get(&b, "/acme/tools")).await,
        "zero-b GET /acme/tools"
    );
    // A stopped regional node falls back to home.
    zero_b.stop().await;
    assert_eq!(
        answered(get(&b, "/acme/tools")).await,
        "zero GET /acme/tools"
    );
    assert_eq!(
        answered(post(&b, "/acme/tools.git/git-upload-pack").body("0000")).await,
        "zero POST /acme/tools.git/git-upload-pack"
    );

    // Region A stops: the cell restarts in its paired region and the operator points its record
    // there. Routers follow once their cached record expires.
    let mut zero = zero;
    zero.stop().await;
    let paired = web_node("zero-paired").await;
    directory
        .register_cell(record(Prefix::ZERO, "a", paired.url(), &zero_key, vec![]))
        .await
        .unwrap();
    expire_routes().await;
    for router in [&a, &b] {
        assert_eq!(
            answered(get(router, "/api/v3/users/alice")).await,
            "zero-paired GET /api/v3/users/alice"
        );
        assert_eq!(
            answered(get(router, "/acme/tools")).await,
            "zero-paired GET /acme/tools"
        );
    }
    listener.stop().await;
}
