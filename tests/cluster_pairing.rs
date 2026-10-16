//! Cluster pairing and the member check, with real iroh endpoints on localhost and
//! PostgreSQL. A missing fixture fails these tests; nothing is skipped.
#![cfg(not(target_arch = "wasm32"))]
#[path = "support/native_database.rs"]
mod support;

use chrono::{Duration as Age, Utc};
use codeyard::backend::cli::cluster::Command;
use codeyard::backend::{
    cluster::{
        self, InviteOptions, JoinTicket, Listen, NodeAddress, NodeKey, Relay, Settings, ALPN_PAIR,
        ALPN_RPC,
    },
    db::{
        self,
        cluster::{Authority, DeclaredMember, JoinRequest, NewJoinToken},
        models::{JoinOutcome, NodeStatusChange},
        DbPool,
    },
    node::NodeRoles,
};
use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, EndpointAddr,
};
use std::time::Duration;
use support::{NativeDatabase, PostgresDatabase};

fn roles(text: &str) -> NodeRoles {
    text.parse().unwrap()
}

fn pg(pool: &DbPool) -> &sqlx::PgPool {
    match pool {
        DbPool::Postgres(pool) => pool,
        DbPool::Sqlite(_) => panic!("this test needs the PostgreSQL fixture"),
    }
}

/// A protocol that only members may reach: it sends back what it receives.
#[derive(Debug, Clone)]
struct Echo;

impl ProtocolHandler for Echo {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection.accept_bi().await?;
        tokio::io::copy(&mut recv, &mut send).await?;
        send.finish()?;
        connection.closed().await;
        Ok(())
    }
}

/// A core node: its cluster listener on a random localhost port, pairing open to
/// everyone and the echo protocol open to members.
struct Core {
    key: NodeKey,
    router: Router,
    address: EndpointAddr,
}

impl Core {
    async fn start(pool: &DbPool) -> Core {
        let key = NodeKey::generate();
        let listen = Listen {
            bind: "127.0.0.1:0".parse().unwrap(),
            relay: Relay::Off,
            advertise: Vec::new(),
        };
        let endpoint = cluster::bind(&listen, &key, pool.clone()).await.unwrap();
        let router = cluster::router(endpoint, cluster::PairingHandler::new(pool.clone()))
            .accept(ALPN_RPC, Echo)
            .spawn();
        cluster::wait_for_address(router.endpoint()).await;
        let address = router.endpoint().addr();
        assert!(
            address.ip_addrs().count() > 0,
            "no local address: {address:?}"
        );
        Core {
            key,
            router,
            address,
        }
    }

    async fn invite(
        &self,
        pool: &DbPool,
        role_list: &str,
        reusable: bool,
        approve: bool,
    ) -> JoinTicket {
        cluster::create_invite(
            pool,
            self.key.public_key(),
            NodeAddress::of(&self.address),
            &InviteOptions {
                roles: roles(role_list),
                reusable,
                requires_approval: approve,
                expires_in: Duration::from_secs(3600),
            },
        )
        .await
        .unwrap()
    }

    async fn stop(self) {
        self.router.shutdown().await.unwrap();
    }
}

async fn join(
    ticket: &JoinTicket,
    key: &NodeKey,
    name: &str,
    wanted: Option<&NodeRoles>,
) -> anyhow::Result<cluster::Membership> {
    cluster::join(ticket, key, &Relay::Off, name, wanted).await
}

/// Connects as `key` to the member-only echo protocol. `Err` carries what the
/// connection reported when it was refused.
async fn try_echo(key: &NodeKey, server: &EndpointAddr) -> Result<Vec<u8>, String> {
    let client: Endpoint = cluster::bind_client(&Relay::Off, key).await.unwrap();
    let attempt = async {
        let connection = client
            .connect(server.clone(), ALPN_RPC)
            .await
            .map_err(|error| format!("{error:?}"))?;
        let echoed = async {
            let (mut send, mut recv) = connection
                .open_bi()
                .await
                .map_err(|error| format!("{error:?}"))?;
            send.write_all(b"hello")
                .await
                .map_err(|error| format!("{error:?}"))?;
            send.finish().map_err(|error| format!("{error:?}"))?;
            recv.read_to_end(64)
                .await
                .map_err(|error| format!("{error:?}"))
        }
        .await;
        connection.close(0u32.into(), b"done");
        echoed
    };
    let outcome = tokio::time::timeout(Duration::from_secs(20), attempt)
        .await
        .unwrap_or_else(|_| Err("timed out".into()));
    client.close().await;
    outcome
}

fn assert_refused(outcome: Result<Vec<u8>, String>) {
    let error = outcome.expect_err("the connection should have been refused");
    assert!(error.contains("403"), "refused for another reason: {error}");
}

async fn token_uses(pool: &DbPool, ticket: &JoinTicket) -> i32 {
    sqlx::query_scalar("SELECT uses FROM join_tokens WHERE token_hash = $1")
        .bind(ticket.secret_hash())
        .fetch_one(pg(pool))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_one_time_invite_adds_one_active_node() {
    let db = PostgresDatabase::new().await;
    let core = Core::start(&db.pool).await;
    let ticket = core.invite(&db.pool, "storage", false, false).await;
    // The command prints the ticket as text; joining needs nothing but that text.
    let ticket = JoinTicket::decode(&ticket.encode()).unwrap();

    let worker = NodeKey::generate();
    let membership = join(&ticket, &worker, "Store-1", None).await.unwrap();
    assert_eq!(membership.name, "store-1");
    assert_eq!(membership.roles, roles("storage"));
    assert_eq!(membership.status, "active");
    assert!(membership.newly_joined);

    let row = db::cluster::get_node_by_public_key(&db.pool, &worker.public_hex())
        .await
        .unwrap()
        .expect("the node is registered");
    assert_eq!(row.status, "active");
    assert_eq!(row.roles, ["storage"]);
    // Joining does not make a node live; its first heartbeat does.
    assert!(row.last_seen.is_none());
    assert_eq!(row.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    // Only the hash of the secret is kept.
    let stored: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM join_tokens")
        .fetch_all(pg(&db.pool))
        .await
        .unwrap();
    assert_eq!(stored, [ticket.secret_hash()]);

    // The new member may use member-only protocols.
    assert_eq!(try_echo(&worker, &core.address).await.unwrap(), b"hello");

    // The ticket is used up.
    let latecomer = NodeKey::generate();
    let error = join(&ticket, &latecomer, "store-2", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not valid"), "{error}");
    assert!(
        db::cluster::get_node_by_public_key(&db.pool, &latecomer.public_hex())
            .await
            .unwrap()
            .is_none()
    );
    assert_refused(try_echo(&latecomer, &core.address).await);

    // The same key may ask again and just hears where it stands.
    let again = join(&ticket, &worker, "store-1", None).await.unwrap();
    assert!(!again.newly_joined);
    assert_eq!(again.status, "active");
    core.stop().await;
}

#[tokio::test]
async fn adopting_takes_only_the_repositories_whose_directories_are_here() {
    let db = PostgresDatabase::new().await;
    let node = db::cluster::register_own_node(
        &db.pool,
        &NodeKey::generate().public_hex(),
        "core",
        &["web".to_owned(), "storage".to_owned()],
        "1",
    )
    .await
    .unwrap();
    let owner = db::users::accounts::create_user(&db.pool, "adopter", None)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let make = |name: &str, present: bool| {
        let path = root.path().join(format!("{}.git", uuid::Uuid::new_v4()));
        if present {
            std::fs::create_dir_all(&path).unwrap();
        }
        let pool = db.pool.clone();
        let (name, path) = (name.to_owned(), path.to_str().unwrap().to_owned());
        async move {
            db::repositories::records::create_repository(&pool, owner.id, &name, None, false, &path)
                .await
                .unwrap()
        }
    };
    let here = make("here", true).await;
    let elsewhere = make("elsewhere", false).await;

    assert_eq!(
        cluster::adopt_repositories(&db.pool, node.id, root.path())
            .await
            .unwrap(),
        1
    );
    let placed = |id| {
        let pool = db.pool.clone();
        async move {
            db::repositories::records::get_repository_by_id(&pool, id)
                .await
                .unwrap()
                .unwrap()
        }
    };
    assert_eq!(placed(here.id).await.storage_node_id, Some(node.id));
    assert_eq!(placed(elsewhere.id).await.storage_node_id, None);
    // Nothing is left to take, and a placed repository never moves.
    assert_eq!(
        cluster::adopt_repositories(&db.pool, node.id, root.path())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_invites_are_listed_and_a_cancelled_one_admits_nobody() {
    let db = PostgresDatabase::new().await;
    let core = Core::start(&db.pool).await;
    let one_time = core.invite(&db.pool, "storage", false, false).await;
    let reusable = core.invite(&db.pool, "web,ssh", true, true).await;

    let open = db::cluster::list_join_tokens(&db.pool).await.unwrap();
    assert_eq!(open.len(), 2);
    assert_eq!(open[0].roles, ["storage"]);
    assert!(!open[0].reusable && !open[0].requires_approval);
    assert_eq!(open[1].roles, ["web", "ssh"]);
    assert!(open[1].reusable && open[1].requires_approval);

    // A one-time invite drops off the list once a node has used it.
    join(&one_time, &NodeKey::generate(), "store-1", None)
        .await
        .unwrap();
    let open = db::cluster::list_join_tokens(&db.pool).await.unwrap();
    assert_eq!(open.len(), 1);
    assert!(open[0].reusable);

    // Cancelling the other one stops new joins; the nodes that joined stay.
    assert!(db::cluster::delete_join_token(&db.pool, open[0].id)
        .await
        .unwrap());
    assert!(!db::cluster::delete_join_token(&db.pool, open[0].id)
        .await
        .unwrap());
    let error = join(&reusable, &NodeKey::generate(), "web-1", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not valid"), "{error}");
    assert!(db::cluster::list_join_tokens(&db.pool)
        .await
        .unwrap()
        .is_empty());
    assert!(db::cluster::get_node_by_name(&db.pool, "store-1")
        .await
        .unwrap()
        .is_some());
    core.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reusable_invite_admits_many_nodes_within_its_roles() {
    let db = PostgresDatabase::new().await;
    let core = Core::start(&db.pool).await;
    let ticket = core.invite(&db.pool, "web,ssh", true, false).await;

    let first = NodeKey::generate();
    let membership = join(&ticket, &first, "web-a", Some(&roles("ssh")))
        .await
        .unwrap();
    assert_eq!(membership.roles, roles("ssh"));
    let second = NodeKey::generate();
    let membership = join(&ticket, &second, "web-b", None).await.unwrap();
    assert_eq!(membership.roles, roles("web,ssh"));
    assert_eq!(token_uses(&db.pool, &ticket).await, 2);
    assert_eq!(try_echo(&first, &core.address).await.unwrap(), b"hello");
    assert_eq!(try_echo(&second, &core.address).await.unwrap(), b"hello");

    // Asking for a role the invite does not allow is refused and costs no use.
    let greedy = NodeKey::generate();
    let error = join(&ticket, &greedy, "web-c", Some(&roles("storage")))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("allows only"), "{error}");
    // So is a name that is taken.
    let error = join(&ticket, &greedy, "web-a", None).await.unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error}");
    assert_eq!(token_uses(&db.pool, &ticket).await, 2);
    // The refused node can still join properly afterwards.
    join(&ticket, &greedy, "web-c", None).await.unwrap();
    assert_eq!(token_uses(&db.pool, &ticket).await, 3);

    // An invite is only good until it expires.
    let expired = JoinTicket::new(core.key.public_key(), NodeAddress::of(&core.address));
    db::cluster::create_join_token(
        &db.pool,
        NewJoinToken {
            token_hash: &expired.secret_hash(),
            roles: &["web".to_owned()],
            reusable: true,
            requires_approval: false,
            expires_at: Utc::now() - Age::minutes(1),
        },
    )
    .await
    .unwrap();
    let error = join(&expired, &NodeKey::generate(), "late", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not valid"), "{error}");
    core.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_needs_approval_waits_as_pending_until_approved() {
    let db = PostgresDatabase::new().await;
    let core = Core::start(&db.pool).await;
    let ticket = core.invite(&db.pool, "actions-worker", false, true).await;

    let worker = NodeKey::generate();
    let membership = join(&ticket, &worker, "ci-1", None).await.unwrap();
    assert_eq!(membership.status, "pending");
    // Pending is not membership.
    assert_refused(try_echo(&worker, &core.address).await);

    let NodeStatusChange::Changed { before, node } =
        cluster::approve(&db.pool, Authority::Operator, "CI-1")
            .await
            .unwrap()
    else {
        panic!("the node should have been approved");
    };
    assert_eq!(
        (before.as_str(), node.status.as_str()),
        ("pending", "active")
    );
    assert_eq!(try_echo(&worker, &core.address).await.unwrap(), b"hello");
    assert_eq!(
        cluster::approve(&db.pool, Authority::Operator, "nobody")
            .await
            .unwrap(),
        NodeStatusChange::Missing
    );
    core.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_a_node_refuses_its_next_connection() {
    let db = PostgresDatabase::new().await;
    let core = Core::start(&db.pool).await;
    let ticket = core.invite(&db.pool, "storage", false, false).await;
    let worker = NodeKey::generate();
    join(&ticket, &worker, "store-1", None).await.unwrap();
    assert_eq!(try_echo(&worker, &core.address).await.unwrap(), b"hello");

    let NodeStatusChange::Changed { before, node } =
        cluster::revoke(&db.pool, Authority::Operator, "store-1")
            .await
            .unwrap()
    else {
        panic!("the node should have been revoked");
    };
    assert_eq!(
        (before.as_str(), node.status.as_str()),
        ("active", "revoked")
    );
    assert_refused(try_echo(&worker, &core.address).await);
    // Revoked nodes cannot pair their way back in either.
    let again = core.invite(&db.pool, "storage", false, false).await;
    let membership = join(&again, &worker, "store-1", None).await.unwrap();
    assert_eq!(membership.status, "revoked");
    assert_refused(try_echo(&worker, &core.address).await);

    // An explicit approval lets it back in.
    cluster::approve(&db.pool, Authority::Operator, "store-1")
        .await
        .unwrap();
    assert_eq!(try_echo(&worker, &core.address).await.unwrap(), b"hello");
    assert_eq!(
        cluster::revoke(&db.pool, Authority::Operator, "nobody")
            .await
            .unwrap(),
        NodeStatusChange::Missing
    );
    core.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_keys_are_refused_everywhere_but_can_only_pair() {
    let db = PostgresDatabase::new().await;
    let core = Core::start(&db.pool).await;
    let stranger = NodeKey::generate();
    assert_refused(try_echo(&stranger, &core.address).await);

    // The pairing door is open to strangers, but only a real secret opens the cluster.
    let forged = JoinTicket::new(core.key.public_key(), NodeAddress::of(&core.address));
    let error = join(&forged, &stranger, "intruder", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not valid"), "{error}");
    assert!(db::cluster::list_nodes(&db.pool).await.unwrap().is_empty());
    assert_refused(try_echo(&stranger, &core.address).await);

    // Garbage and oversized requests are answered or dropped without side effects.
    let client = cluster::bind_client(&Relay::Off, &stranger).await.unwrap();
    let answer = raw_pairing(&client, &core.address, b"this is not json")
        .await
        .unwrap();
    let answer = String::from_utf8(answer).unwrap();
    assert!(
        answer.contains("refused") && answer.contains("could not be read"),
        "{answer}"
    );
    let error = raw_pairing(&client, &core.address, &vec![b'x'; 10_000])
        .await
        .expect_err("an oversized pairing request must be closed by the server");
    assert!(
        error.contains("bad pairing request"),
        "unexpected refusal: {error}"
    );
    client.close().await;
    assert!(db::cluster::list_nodes(&db.pool).await.unwrap().is_empty());
    core.stop().await;
}

/// Pairing serves sixteen peers at once. Peers that connect and open no stream hold the
/// slots, a seventeenth is refused at once with the overload answer, and the slots come
/// back when the idle peers drop.
#[tokio::test]
async fn idle_pairing_peers_hold_their_slots_and_free_them_when_they_drop() {
    let db = PostgresDatabase::new().await;
    let core = Core::start(&db.pool).await;
    let mut idle = Vec::new();
    for _ in 0..16 {
        let client = cluster::bind_client(&Relay::Off, &NodeKey::generate())
            .await
            .unwrap();
        let connection = client
            .connect(core.address.clone(), ALPN_PAIR)
            .await
            .unwrap();
        idle.push((client, connection));
    }
    // Each connection takes its slot on the task that serves it, so give those tasks a moment.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = cluster::bind_client(&Relay::Off, &NodeKey::generate())
        .await
        .unwrap();
    let connection = client
        .connect(core.address.clone(), ALPN_PAIR)
        .await
        .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), connection.closed())
        .await
        .expect("the seventeenth peer is refused at once");
    match closed {
        iroh::endpoint::ConnectionError::ApplicationClosed(close) => {
            assert_eq!(u64::from(close.error_code), 503);
            assert_eq!(&close.reason[..], b"too many pairing requests");
        }
        other => panic!("the seventeenth peer was not refused with the overload answer: {other}"),
    }
    client.close().await;

    // The idle peers hang up, which frees their slots: a new peer gets answers again.
    for (client, connection) in idle.drain(..) {
        connection.close(0u32.into(), b"idle peer done");
        client.close().await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let client = cluster::bind_client(&Relay::Off, &NodeKey::generate())
            .await
            .unwrap();
        let answer = raw_pairing(&client, &core.address, b"this is not json").await;
        client.close().await;
        if answer
            .as_ref()
            .is_ok_and(|bytes| String::from_utf8_lossy(bytes).contains("could not be read"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pairing slots were not released: {answer:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    core.stop().await;
}

async fn raw_pairing(
    client: &Endpoint,
    server: &EndpointAddr,
    request: &[u8],
) -> Result<Vec<u8>, String> {
    let attempt = async {
        let connection = client
            .connect(server.clone(), ALPN_PAIR)
            .await
            .map_err(|error| format!("{error:?}"))?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|error| format!("{error:?}"))?;
        send.write_all(request)
            .await
            .map_err(|error| format!("{error:?}"))?;
        send.finish().map_err(|error| format!("{error:?}"))?;
        let answer = recv
            .read_to_end(8192)
            .await
            .map_err(|error| format!("{error:?}"));
        connection.close(0u32.into(), b"done");
        answer
    };
    tokio::time::timeout(Duration::from_secs(20), attempt)
        .await
        .unwrap_or_else(|_| Err("timed out".into()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_nodes_are_members_without_a_join_step() {
    let db = PostgresDatabase::new().await;
    let core = Core::start(&db.pool).await;
    let declared = NodeKey::generate();
    assert_refused(try_echo(&declared, &core.address).await);

    let list = parse_nodes(&format!("{}:storage+web:Store-9", declared.public_hex()));
    let summary = cluster::apply_declared_nodes(&db.pool, &list)
        .await
        .unwrap();
    assert_eq!((summary.applied, summary.kept_revoked.len()), (1, 0));
    let row = db::cluster::get_node_by_name(&db.pool, "store-9")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.status.as_str(), row.roles.as_slice()),
        (
            "active",
            ["web".to_owned(), "storage".to_owned()].as_slice()
        )
    );
    assert_eq!(try_echo(&declared, &core.address).await.unwrap(), b"hello");

    // Applying the list again is harmless, and edits to it take effect.
    cluster::apply_declared_nodes(&db.pool, &list)
        .await
        .unwrap();
    let edited = parse_nodes(&format!("{}:ssh:renamed", declared.public_hex()));
    cluster::apply_declared_nodes(&db.pool, &edited)
        .await
        .unwrap();
    let nodes = db::cluster::list_nodes(&db.pool).await.unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(
        (nodes[0].name.as_str(), nodes[0].roles.as_slice()),
        ("renamed", ["ssh".to_owned()].as_slice())
    );

    // A revoke is not undone by the list: it stays revoked until someone approves it.
    cluster::revoke(&db.pool, Authority::Operator, "renamed")
        .await
        .unwrap();
    let summary = cluster::apply_declared_nodes(&db.pool, &edited)
        .await
        .unwrap();
    assert_eq!(
        (summary.applied, summary.kept_revoked.as_slice()),
        (0, ["renamed".to_owned()].as_slice())
    );
    assert_refused(try_echo(&declared, &core.address).await);
    cluster::approve(&db.pool, Authority::Operator, "renamed")
        .await
        .unwrap();
    assert_eq!(try_echo(&declared, &core.address).await.unwrap(), b"hello");

    // A name that belongs to another key is reported, not silently shared.
    let other = NodeKey::generate();
    let clash = parse_nodes(&format!("{}:web:renamed", other.public_hex()));
    let error = cluster::apply_declared_nodes(&db.pool, &clash)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("already uses the name"),
        "{error:#}"
    );
    // The list is applied in one transaction: the valid entry, listed first, is not applied.
    let fresh = NodeKey::generate();
    let mixed = parse_nodes(&format!(
        "{}:web:fresh-one\n{}:web:renamed",
        fresh.public_hex(),
        other.public_hex()
    ));
    cluster::apply_declared_nodes(&db.pool, &mixed)
        .await
        .unwrap_err();
    assert!(
        db::cluster::get_node_by_public_key(&db.pool, &fresh.public_hex())
            .await
            .unwrap()
            .is_none()
    );
    core.stop().await;
}

fn parse_nodes(text: &str) -> Vec<cluster::DeclaredNode> {
    cluster::parse_declared_nodes(text).unwrap()
}

fn member(public_key: &str, name: &str, roles: &[String]) -> DeclaredMember {
    DeclaredMember {
        public_key: public_key.to_owned(),
        name: name.to_owned(),
        roles: roles.to_vec(),
    }
}

fn settings(name: &str, ticket: Option<JoinTicket>) -> Settings {
    Settings {
        bind: Some("127.0.0.1:0".parse().unwrap()),
        relay: Relay::Off,
        advertise: Vec::new(),
        tcp_bind: None,
        tcp_advertise: Vec::new(),
        name: name.to_owned(),
        declared: Vec::new(),
        join_ticket: ticket,
        peers: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serving_nodes_register_report_their_address_and_pair_with_a_token() {
    let db = PostgresDatabase::new().await;
    let state_a = tempfile::tempdir().unwrap();
    let state_b = tempfile::tempdir().unwrap();

    // A core node registers itself and publishes where it can be reached.
    let core = cluster::start(
        &db.pool,
        state_a.path(),
        state_a.path(),
        &roles("web,ssh,storage"),
        settings("core-a", None),
    )
    .await
    .unwrap();
    let key_a = cluster::load_existing(state_a.path())
        .unwrap()
        .expect("the key was saved");
    let row = db::cluster::get_node_by_public_key(&db.pool, &key_a.public_hex())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.name.as_str(), row.status.as_str()),
        ("core-a", "active")
    );
    assert_eq!(row.roles, ["web", "storage", "ssh"]);
    // Registering is not publishing: the node is not live until its services are up.
    assert!(row.last_seen.is_none() && row.addr_hint.is_none());
    core.publish().await.unwrap();
    let row = db::cluster::get_node_by_public_key(&db.pool, &key_a.public_hex())
        .await
        .unwrap()
        .unwrap();
    assert!(row.last_seen.is_some() && row.addr_hint.is_some());
    let reported = codeyard::backend::cli::cluster::own_address(&db.pool, &key_a)
        .await
        .unwrap();
    assert!(!reported.addrs.is_empty());
    assert_eq!(
        reported.addrs,
        NodeAddress::of(&core.endpoint().unwrap().addr()).addrs
    );

    // Placement picks the published storage node, and no other.
    let placed = codeyard::backend::cluster::placement::storage_node(Some(&core.context), None)
        .await
        .unwrap();
    assert_eq!(placed, Some(row.id));
    // Shutdown withdraws it: placement stops choosing it at once.
    core.withdraw().await;
    let row = db::cluster::get_node_by_public_key(&db.pool, &key_a.public_hex())
        .await
        .unwrap()
        .unwrap();
    assert!(row.last_seen.is_none() && !row.is_online());
    assert!(
        codeyard::backend::cluster::placement::storage_node(Some(&core.context), None)
            .await
            .is_err()
    );

    // `cluster invite` builds its ticket from that reported address.
    let ticket = cluster::create_invite(
        &db.pool,
        key_a.public_key(),
        reported,
        &InviteOptions {
            roles: roles("storage,ssh"),
            reusable: false,
            requires_approval: false,
            expires_in: Duration::from_secs(600),
        },
    )
    .await
    .unwrap();

    // A second node starts with the ticket as its join token and pairs itself.
    let worker = cluster::start(
        &db.pool,
        state_b.path(),
        state_b.path(),
        &roles("storage"),
        settings("store-b", Some(ticket.clone())),
    )
    .await
    .unwrap();
    let key_b = cluster::load_existing(state_b.path()).unwrap().unwrap();
    let row = db::cluster::get_node_by_public_key(&db.pool, &key_b.public_hex())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.name.as_str(), row.status.as_str()),
        ("store-b", "active")
    );
    assert_eq!(row.roles, ["storage"]);
    assert_eq!(token_uses(&db.pool, &ticket).await, 1);
    worker.shutdown().await;
    drop(worker);

    // After a restart the saved key is recognised: no second join, and the used-up
    // one-time token is not needed.
    let worker = cluster::start(
        &db.pool,
        state_b.path(),
        state_b.path(),
        &roles("storage"),
        settings("store-b", Some(ticket.clone())),
    )
    .await
    .unwrap();
    assert_eq!(token_uses(&db.pool, &ticket).await, 1);
    assert_eq!(db::cluster::list_nodes(&db.pool).await.unwrap().len(), 2);
    worker.shutdown().await;
    drop(worker);

    // A revoked node does not start in cluster mode.
    cluster::revoke(&db.pool, Authority::Operator, "store-b")
        .await
        .unwrap();
    let error = match cluster::start(
        &db.pool,
        state_b.path(),
        state_b.path(),
        &roles("storage"),
        settings("store-b", None),
    )
    .await
    {
        Ok(_) => panic!("a revoked node must not start"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("revoked"), "{error}");

    // A node that waits for approval still starts, as pending.
    let pending_ticket = cluster::create_invite(
        &db.pool,
        key_a.public_key(),
        codeyard::backend::cli::cluster::own_address(&db.pool, &key_a)
            .await
            .unwrap(),
        &InviteOptions {
            roles: roles("web"),
            reusable: false,
            requires_approval: true,
            expires_in: Duration::from_secs(600),
        },
    )
    .await
    .unwrap();
    let state_c = tempfile::tempdir().unwrap();
    let waiting = cluster::start(
        &db.pool,
        state_c.path(),
        state_c.path(),
        &roles("web"),
        settings("web-c", Some(pending_ticket)),
    )
    .await
    .unwrap();
    let pending = db::cluster::get_node_by_name(&db.pool, "web-c")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.status, "pending");
    waiting.shutdown().await;
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advertised_addresses_are_what_peers_are_told_to_dial() {
    // The endpoint only keeps the pool for its member check, so any pool will do here.
    let database = NativeDatabase::new().await;
    let advertise = cluster::resolve_advertised(&[
        "203.0.113.5:7444".to_owned(),
        "[2001:db8::1]:7444".to_owned(),
    ])
    .await
    .unwrap();
    assert_eq!(advertise.len(), 2);
    assert!(cluster::resolve_advertised(&["no port here".to_owned()])
        .await
        .is_err());

    let listen = Listen {
        bind: "127.0.0.1:0".parse().unwrap(),
        relay: Relay::Off,
        advertise: advertise.clone(),
    };
    let endpoint = cluster::bind(&listen, &NodeKey::generate(), database.pool.clone())
        .await
        .unwrap();
    cluster::wait_for_address(&endpoint).await;
    let told = NodeAddress::of(&endpoint.addr());
    for address in &advertise {
        assert!(
            told.addrs.contains(address),
            "{address} missing from {told:?}"
        );
    }
    // The address it really listens on is still there for peers on the same network.
    assert!(told.addrs.iter().any(|address| address.ip().is_loopback()));
    assert!(
        told.relays.is_empty(),
        "no relay unless IROH_RELAY asks for one"
    );
    endpoint.close().await;
    database.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_installations_refuse_every_cluster_action() {
    let sqlite = NativeDatabase::new().await;
    let directory = tempfile::tempdir().unwrap();
    let refused = |error: anyhow::Error| {
        let text = format!("{error:#}");
        assert!(text.contains("PostgreSQL"), "{text}");
    };
    refused(cluster::require_postgres(&sqlite.pool).unwrap_err());
    let commands = [
        Command::List,
        Command::Approve { name: "x".into() },
        Command::Revoke { name: "x".into() },
        Command::Invite {
            roles: roles("storage"),
            reusable: false,
            expires: Duration::from_secs(60),
            approve: false,
        },
    ];
    for command in &commands {
        refused(
            codeyard::backend::cli::cluster::run(
                command,
                &sqlite.pool,
                directory.path(),
                directory.path(),
            )
            .await
            .unwrap_err(),
        );
    }
    // Starting refuses before it creates a key or opens a socket.
    let result = cluster::start(
        &sqlite.pool,
        directory.path(),
        directory.path(),
        &roles("web,storage"),
        settings("solo", None),
    )
    .await;
    refused(result.err().expect("SQLite cannot be clustered"));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);

    // Joining from an installation that uses SQLite is refused too.
    let join = Command::Join {
        ticket: "anything".into(),
        name: None,
    };
    refused(
        codeyard::backend::cli::cluster::run_offline(
            &join,
            directory.path(),
            Some("sqlite://data/gitserv.db"),
        )
        .await
        .unwrap_err(),
    );
    std::fs::write(directory.path().join("gitserv.db"), b"").unwrap();
    refused(
        codeyard::backend::cli::cluster::run_offline(&join, directory.path(), None)
            .await
            .unwrap_err(),
    );
    // With no SQLite in sight the same command gets as far as reading the ticket.
    let clean = tempfile::tempdir().unwrap();
    let error = codeyard::backend::cli::cluster::run_offline(&join, clean.path(), None)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("not a Codeyard join ticket"),
        "{error}"
    );
    sqlite.close().await;
}

/// The same operations must give the same answers on both engines.
async fn membership_contract(pool: &DbPool) {
    let hash = |byte: u8| cluster::hash_secret(&[byte; 32]);
    let key = |_: u8| NodeKey::generate().public_hex();
    let storage = ["storage".to_owned()];
    let token = |hash: &str, roles: &[String], reusable: bool, approval: bool, expires_at| {
        let hash = hash.to_owned();
        let roles = roles.to_vec();
        async move {
            db::cluster::create_join_token(
                pool,
                NewJoinToken {
                    token_hash: &hash,
                    roles: &roles,
                    reusable,
                    requires_approval: approval,
                    expires_at,
                },
            )
            .await
        }
    };
    let soon = Utc::now() + Age::hours(1);
    token(&hash(1), &storage, false, false, soon).await.unwrap();
    assert!(
        token(&hash(1), &storage, false, false, soon).await.is_err(),
        "hashes are unique"
    );

    let request = |token: &str, public_key: &str, name: &str, wanted: Option<&[String]>| {
        let (token, public_key, name, wanted) = (
            token.to_owned(),
            public_key.to_owned(),
            name.to_owned(),
            wanted.map(<[String]>::to_vec),
        );
        async move {
            db::cluster::join_with_token(
                pool,
                JoinRequest {
                    token_hash: &token,
                    public_key: &public_key,
                    name: &name,
                    roles: wanted.as_deref(),
                    version: "1.2.3",
                },
            )
            .await
            .unwrap()
        }
    };
    let (key_a, key_b, key_c) = (key(1), key(2), key(3));
    let JoinOutcome::Joined(node) = request(&hash(1), &key_a, "a", None).await else {
        panic!("the first join should succeed");
    };
    assert_eq!((node.name.as_str(), node.status.as_str()), ("a", "active"));
    assert_eq!(node.roles, storage);
    assert_eq!(node.version.as_deref(), Some("1.2.3"));
    assert!(node.last_seen.is_none() && node.addr_hint.is_none() && node.disk_free.is_none());
    assert_eq!(
        request(&hash(1), &key_b, "b", None).await,
        JoinOutcome::InvalidToken
    );
    assert_eq!(
        request(&hash(9), &key_b, "b", None).await,
        JoinOutcome::InvalidToken
    );
    let JoinOutcome::AlreadyMember(known) = request(&hash(1), &key_a, "other", None).await else {
        panic!("a known key keeps its standing");
    };
    assert_eq!(known.name, "a");

    // Reusable, approval, role limits, names.
    token(
        &hash(2),
        &["web".to_owned(), "ssh".to_owned()],
        true,
        true,
        soon,
    )
    .await
    .unwrap();
    let wanted = ["ssh".to_owned()];
    let JoinOutcome::Joined(pending) = request(&hash(2), &key_b, "b", Some(&wanted)).await else {
        panic!("reusable join");
    };
    assert_eq!(
        (pending.status.as_str(), pending.roles.as_slice()),
        ("pending", ["ssh".to_owned()].as_slice())
    );
    let outside = ["storage".to_owned()];
    match request(&hash(2), &key_c, "c", Some(&outside)).await {
        JoinOutcome::RolesNotAllowed { allowed } => assert_eq!(allowed, ["web", "ssh"]),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        request(&hash(2), &key_c, "a", None).await,
        JoinOutcome::NameTaken
    );
    let JoinOutcome::Joined(_) = request(&hash(2), &key_c, "c", None).await else {
        panic!("a refused request must not have used the token");
    };
    token(
        &hash(3),
        &storage,
        true,
        false,
        Utc::now() - Age::minutes(1),
    )
    .await
    .unwrap();
    assert_eq!(
        request(&hash(3), &key(4), "d", None).await,
        JoinOutcome::InvalidToken
    );

    // Lookups, the member check, approve and revoke.
    assert!(db::cluster::is_active_node(pool, &key_a).await.unwrap());
    assert!(
        !db::cluster::is_active_node(pool, &key_b).await.unwrap(),
        "pending is not active"
    );
    assert!(!db::cluster::is_active_node(pool, &key(5)).await.unwrap());
    // Membership changes check the acting administrator where they are written.
    let plain = db::users::accounts::create_user_with_password_and_flags(
        pool,
        "node_plain",
        None,
        "hash",
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        db::cluster::set_node_status(pool, Authority::Administrator(plain.id), "b", "active")
            .await
            .unwrap(),
        NodeStatusChange::Denied
    );
    assert!(!db::cluster::is_active_node(pool, &key_b).await.unwrap());
    let admin = db::users::accounts::create_user_with_password_and_flags(
        pool,
        "node_admin",
        None,
        "hash",
        true,
        false,
    )
    .await
    .unwrap();
    let banned = db::users::accounts::create_user_with_password_and_flags(
        pool,
        "node_banned",
        None,
        "hash",
        true,
        false,
    )
    .await
    .unwrap();
    assert!(matches!(
        db::users::transitions::change_ban(pool, admin.id, banned.id, true)
            .await
            .unwrap(),
        db::users::transitions::BanChange::Updated(_)
    ));
    assert_eq!(
        db::cluster::set_node_status(pool, Authority::Administrator(banned.id), "b", "active")
            .await
            .unwrap(),
        NodeStatusChange::Denied
    );
    assert!(!db::cluster::is_active_node(pool, &key_b).await.unwrap());

    let NodeStatusChange::Changed {
        before,
        node: approved,
    } = db::cluster::set_node_status(pool, Authority::Administrator(admin.id), "b", "active")
        .await
        .unwrap()
    else {
        panic!("an active administrator may approve");
    };
    assert_eq!(
        (before.as_str(), approved.status.as_str()),
        ("pending", "active")
    );
    assert!(db::cluster::is_active_node(pool, &key_b).await.unwrap());
    let NodeStatusChange::Changed { node: revoked, .. } =
        db::cluster::set_node_status(pool, Authority::Operator, "b", "revoked")
            .await
            .unwrap()
    else {
        panic!("the operator may revoke");
    };
    assert_eq!(revoked.status, "revoked");
    assert!(!db::cluster::is_active_node(pool, &key_b).await.unwrap());
    assert_eq!(
        db::cluster::set_node_status(pool, Authority::Operator, "missing", "active")
            .await
            .unwrap(),
        NodeStatusChange::Missing
    );
    let names: Vec<String> = db::cluster::list_nodes(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|node| node.name)
        .collect();
    assert_eq!(names, ["a", "b", "c"]);

    // Heartbeat: only a known key, and it records the address.
    assert!(
        db::cluster::record_node_heartbeat(pool, &key_a, "2.0.0", "{\"addrs\":[]}", None)
            .await
            .unwrap()
    );
    assert!(
        !db::cluster::record_node_heartbeat(pool, &key(6), "2.0.0", "{}", None)
            .await
            .unwrap()
    );
    let beat = db::cluster::get_node_by_name(pool, "a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (beat.version.as_deref(), beat.addr_hint.as_deref()),
        (Some("2.0.0"), Some("{\"addrs\":[]}"))
    );
    assert!(beat.is_online());
    // Withdrawing clears the sign of life and keeps the last address.
    db::cluster::clear_node_heartbeat(pool, &key_a)
        .await
        .unwrap();
    let cleared = db::cluster::get_node_by_name(pool, "a")
        .await
        .unwrap()
        .unwrap();
    assert!(cleared.last_seen.is_none() && !cleared.is_online());
    assert_eq!(cleared.addr_hint.as_deref(), beat.addr_hint.as_deref());

    // A node with database access registers itself once and keeps what it has.
    let own = key(7);
    let first = db::cluster::register_own_node(
        pool,
        &own,
        "core",
        &["web".to_owned(), "ssh".to_owned()],
        "1",
    )
    .await
    .unwrap();
    assert_eq!(
        (first.status.as_str(), first.roles.as_slice()),
        ("active", ["web".to_owned(), "ssh".to_owned()].as_slice())
    );
    assert!(first.last_seen.is_none(), "registering is not publishing");
    db::cluster::set_node_status(pool, Authority::Operator, "core", "revoked")
        .await
        .unwrap();
    let second = db::cluster::register_own_node(pool, &own, "renamed", &storage, "2")
        .await
        .unwrap();
    assert_eq!(
        (
            second.name.as_str(),
            second.status.as_str(),
            second.version.as_deref()
        ),
        ("core", "revoked", Some("2"))
    );
    assert_eq!(second.roles, first.roles);
    let error = db::cluster::register_own_node(pool, &key(8), "core", &storage, "1")
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("already uses the name"),
        "{error}"
    );

    // The declared list sets name and roles, activates, and never undoes a revoke.
    let declared = key(9);
    let node = db::cluster::upsert_declared_nodes(pool, &[member(&declared, "decl", &storage)])
        .await
        .unwrap()
        .remove(0);
    assert_eq!(node.status, "active");
    let web = ["web".to_owned()];
    let node = db::cluster::upsert_declared_nodes(pool, &[member(&declared, "decl2", &web)])
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        (node.name.as_str(), node.roles.as_slice()),
        ("decl2", web.as_slice())
    );
    db::cluster::set_node_status(pool, Authority::Operator, "decl2", "revoked")
        .await
        .unwrap();
    let node = db::cluster::upsert_declared_nodes(pool, &[member(&declared, "decl2", &web)])
        .await
        .unwrap()
        .remove(0);
    assert_eq!(node.status, "revoked");
    assert!(
        db::cluster::upsert_declared_nodes(pool, &[member(&key(10), "decl2", &storage)])
            .await
            .is_err()
    );

    // A list with a failing entry applies none of its entries.
    let fresh = key(11);
    let batch = [
        member(&fresh, "batch-new", &storage),
        member(&key(12), "decl2", &storage),
    ];
    assert!(db::cluster::upsert_declared_nodes(pool, &batch)
        .await
        .is_err());
    assert!(db::cluster::get_node_by_public_key(pool, &fresh)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn membership_queries_agree_on_sqlite_and_postgresql() {
    let sqlite = NativeDatabase::new().await;
    membership_contract(&sqlite.pool).await;
    sqlite.close().await;
    let postgres = PostgresDatabase::new().await;
    membership_contract(&postgres.pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withdrawing_gives_up_when_the_node_row_is_locked() {
    let db = PostgresDatabase::new().await;
    let state = tempfile::tempdir().unwrap();
    let core = cluster::start(
        &db.pool,
        state.path(),
        state.path(),
        &roles("web"),
        settings("locked-node", None),
    )
    .await
    .unwrap();
    core.publish().await.unwrap();
    let key = cluster::load_existing(state.path())
        .unwrap()
        .expect("the key was saved");

    // Another transaction holds this node's row, as a write stalled elsewhere would.
    let mut holder = pg(&db.pool).begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE public_key = $1 FOR UPDATE")
        .bind(key.public_hex())
        .fetch_one(&mut *holder)
        .await
        .unwrap();

    // Shutdown must not wait for the row: the withdrawal gives up after its own bound.
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(30), core.withdraw())
        .await
        .expect("withdrawing waited for the locked row");
    assert!(started.elapsed() < Duration::from_secs(10));

    holder.rollback().await.unwrap();
    core.shutdown().await;
}
