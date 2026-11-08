//! Regions through the real router: signups land in the router's region, accounts keep their
//! home, an organization with presence reads from its home cell's web nodes in that region and
//! writes at home, and a cell moved to its paired region is served there. Counted member
//! requests add and remove an organization's presence.
use super::cells_frontend::Server;
use crate::support::native_database::PostgresDatabase;
use codeyard_db_directory::Directory;
use codeyard_router::{
    directory, frontend,
    http::Transport,
    routes::{self, Routes},
};
use codeyard_security::node_identity::NodeKey;
use codeyard_types::cell_ids::Prefix;
use codeyard_types::cells::{Cell, Key, KeyKind, Object, ObjectKind, ReadRegion, State};
use std::time::Duration;

/// A stand-in cell web node that names itself in every response. On `/alice/stamp` it also
/// names the region the router stamped. It reads the whole body first, as a real node does,
/// so a large forwarded body is never cut off by an early reply.
async fn web_node(name: &'static str) -> Server {
    Server::start(axum::Router::new().fallback(
        move |method: axum::http::Method,
              uri: axum::http::Uri,
              headers: axum::http::HeaderMap,
              _body: axum::body::Bytes| async move {
            let mut reply = format!("{name} {method} {}", uri.path());
            if uri.path() == "/alice/stamp" {
                for region in headers.get_all(codeyard_types::cells::REGION_HEADER) {
                    reply.push(' ');
                    reply.push_str(region.to_str().unwrap());
                }
            }
            reply
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
    let (client, request) = request.build_split();
    let request = request.unwrap();
    let target = format!("{} {}", request.method(), request.url());
    let response = client.execute(request).await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "{target}: {body}");
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
    // Routers start before the cells register: signup placement only takes a cell whose
    // record is fresh, and building a router can take a while on a loaded runner.
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
    // Routers name their own region, replacing one the client sent.
    for (router, region) in [(&a, "a"), (&b, "b")] {
        assert_eq!(
            answered(
                get(router, "/alice/stamp").header(codeyard_types::cells::REGION_HEADER, "evil")
            )
            .await,
            format!("zero GET /alice/stamp {region}")
        );
    }
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

async fn counted(pool: &codeyard_database::pool::PgPool) -> Vec<(i64, String, i64)> {
    codeyard_database::cells::regions::flush(pool)
        .await
        .unwrap();
    sqlx::query_as(
        "SELECT user_id,region,requests FROM cell_request_counts ORDER BY user_id,region",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Sets each of the last seven days' counts for one account and region.
async fn set_counts(
    pool: &codeyard_database::pool::PgPool,
    user: i64,
    region: &str,
    requests: i64,
) {
    sqlx::query("DELETE FROM cell_request_counts WHERE user_id=$1 AND region=$2")
        .bind(user)
        .bind(region)
        .execute(pool)
        .await
        .unwrap();
    if requests > 0 {
        sqlx::query("INSERT INTO cell_request_counts(user_id,region,day,requests) SELECT $1,$2,(clock_timestamp() AT TIME ZONE 'UTC')::date-g,$3 FROM generate_series(0,6) AS g")
            .bind(user)
            .bind(region)
            .bind(requests)
            .execute(pool)
            .await
            .unwrap();
    }
}

async fn local_presence(pool: &codeyard_database::pool::PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT region FROM cell_presence ORDER BY region")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn audits(pool: &codeyard_database::pool::PgPool, action: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE action=$1")
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn counted_member_requests_add_and_remove_organization_presence() {
    use codeyard_db_placement::regions::{self, Change};
    use codeyard_types::cluster::RegionPolicy;
    crate::support::fake_webui_bundle::install();
    let registry = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&registry.connection_url(), Default::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    let zero = PostgresDatabase::new().await;
    let codeyard_database::DbPool::Postgres(pool) = &zero.pool else {
        unreachable!()
    };
    let (mut alice_server, alice, alice_token) =
        super::cells_frontend::native(&zero, "alice").await;
    let acme = crate::support::organizations::applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            &zero.pool,
            alice,
            &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
                "acme",
                codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    let [zero_key, directory_key] = [(); 2].map(|()| NodeKey::generate());
    directory
        .register_cell(record(
            Prefix::ZERO,
            "a",
            alice_server.url(),
            &zero_key,
            vec![],
        ))
        .await
        .unwrap();
    let object = Object {
        kind: ObjectKind::Organization,
        id: acme,
    };
    let keys = [Key::new(KeyKind::Name, "acme").unwrap()];
    let nonce = "d".repeat(64);
    directory
        .reserve(&zero_key.public_hex(), object, &keys, &nonce)
        .await
        .unwrap();
    directory
        .confirm(&zero_key.public_hex(), object, &keys, &nonce)
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
    zero.pool
        .set_directory(std::sync::Arc::new(
            directory::Client::new(
                zero_key.clone(),
                listener.address,
                directory_key.public_key(),
                Default::default(),
            )
            .unwrap(),
        ))
        .unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();

    // A region header from a peer that is not a trusted proxy is dropped, so nothing counts.
    let response = client
        .get(format!("{}/api/v3/user", alice_server.url()))
        .bearer_auth(&alice_token)
        .header(codeyard_types::cells::REGION_HEADER, "b")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(counted(pool).await.is_empty());

    // Behind the stamp, only requests that authenticate count, under the stamped region.
    let root = tempfile::tempdir().unwrap();
    let state = codeyard_app::app::AppState::new(
        zero.pool.clone(),
        root.path().into(),
        std::sync::Arc::new(
            codeyard_storage::ObjectStore::open(root.path(), None)
                .await
                .unwrap(),
        ),
        None,
        Default::default(),
        None,
    );
    let mut counting = Server::start(
        axum::Router::new()
            .fallback(|| async { "ok" })
            .layer(axum::middleware::from_fn(
                codeyard_api_http::middleware::count_regional_requests,
            ))
            .layer(axum::Extension(std::sync::Arc::new(state))),
    )
    .await;
    let send = |token: Option<&str>| {
        let request = client
            .get(format!("{}/", counting.url()))
            .header(codeyard_types::cells::REGION_HEADER, "b");
        let request = match token {
            Some(token) => request.bearer_auth(token),
            None => request,
        };
        request.send()
    };
    let wrong = zero
        .pool
        .cell_prefix()
        .token(codeyard_types::tokens::PAT_PREFIX, &"e".repeat(64))
        .unwrap();
    for token in [None, Some(wrong.as_str()), Some(alice_token.as_str())] {
        assert_eq!(send(token).await.unwrap().status(), 200);
    }
    assert_eq!(counted(pool).await, [(alice, "b".to_owned(), 1)]);

    // The default policy is paused and dry run. Saving needs an administrator.
    let mut policy = regions::policy(pool).await.unwrap();
    assert_eq!(policy, RegionPolicy::default());
    assert!(regions::save(pool, &alice.into(), &policy).await.is_err());
    assert!(regions::due(pool).await.unwrap().is_some());
    assert!(regions::due(pool).await.unwrap().is_none());
    set_counts(pool, alice, "a", 300).await;
    set_counts(pool, alice, "b", 700).await;
    assert!(regions::evaluate(pool, &policy).await.unwrap().is_empty());

    // Dry run only records what it would do.
    policy.paused = false;
    let would = Change {
        organization: acme,
        region: "b".into(),
        present: true,
        applied: false,
    };
    assert_eq!(regions::evaluate(pool, &policy).await.unwrap(), [would]);
    assert!(directory.presence(acme).await.unwrap().is_empty());
    assert!(local_presence(pool).await.is_empty());
    assert_eq!(audits(pool, "regions.presence_would_add").await, 1);

    // Live, region B is added. The home region never becomes a presence region.
    policy.dry_run = false;
    assert_eq!(
        regions::evaluate(pool, &policy).await.unwrap(),
        [Change {
            organization: acme,
            region: "b".into(),
            present: true,
            applied: true,
        }]
    );
    assert_eq!(directory.presence(acme).await.unwrap(), ["b"]);
    assert_eq!(local_presence(pool).await, ["b"]);
    assert_eq!(audits(pool, "regions.presence_added").await, 1);
    assert!(regions::evaluate(pool, &policy).await.unwrap().is_empty());

    // Requests from B stop: once the presence is old enough, B is removed. Presence set by
    // hand stays in the directory.
    assert!(directory.set_presence(acme, "c", true).await.unwrap());
    sqlx::query("INSERT INTO cell_presence(organization_id,region) VALUES($1,'c')")
        .bind(acme)
        .execute(pool)
        .await
        .unwrap();
    set_counts(pool, alice, "b", 0).await;
    assert!(regions::evaluate(pool, &policy).await.unwrap().is_empty());
    sqlx::query("UPDATE cell_presence SET added_at=clock_timestamp()-INTERVAL '30 days'")
        .execute(pool)
        .await
        .unwrap();
    let removed = regions::evaluate(pool, &policy).await.unwrap();
    assert_eq!(
        removed
            .iter()
            .map(|change| (change.region.as_str(), change.present, change.applied))
            .collect::<Vec<_>>(),
        [("b", false, true), ("c", false, false)]
    );
    assert_eq!(directory.presence(acme).await.unwrap(), ["c"]);
    assert!(local_presence(pool).await.is_empty());
    assert_eq!(audits(pool, "regions.presence_removed").await, 1);

    // The directory refuses presence for an organization another cell owns.
    let other = NodeKey::generate();
    directory
        .register_cell(record(
            Prefix::new(1),
            "b",
            alice_server.url(),
            &other,
            vec![],
        ))
        .await
        .unwrap();
    assert!(directory
        .set_presence_for(&other.public_hex(), acme, "d", true)
        .await
        .is_err());
    assert_eq!(directory.presence(acme).await.unwrap(), ["c"]);
    counting.stop().await;
    alice_server.stop().await;
    listener.stop().await;
}
