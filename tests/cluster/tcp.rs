//! Node connections over TCP and TLS, with real sockets on loopback and one PostgreSQL
//! database. Most nodes here have no iroh socket and are reachable only through
//! `CLUSTER_TCP_BIND`; the dial tests start nodes on iroh as well.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::native_database as native;

use codeyard::backend::{
    ci::{queue::RunQueue, CiExecutionMode},
    cluster::{
        self, link::Link, rpc, tcp, InviteOptions, IrohDialer, JoinTicket, NodeAddress, NodeKey,
        Relay, Settings, ALPN_EVENTS, ALPN_PAIR, ALPN_RPC,
    },
    db,
};
use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_rustls::{
    rustls::{
        client::{
            danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
            AlwaysResolvesClientRawPublicKeys,
        },
        crypto::{aws_lc_rs, verify_tls13_signature_with_raw_key, CryptoProvider},
        pki_types::{
            CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer,
            UnixTime,
        },
        sign::CertifiedKey,
        version::{TLS12, TLS13},
        ClientConfig, DigitallySignedStruct, Error, SignatureScheme, SupportedProtocolVersion,
    },
    TlsConnector,
};
use uuid::Uuid;

/// The RFC 8410 headers of an ed25519 SubjectPublicKeyInfo and PKCS#8 key.
const ED25519_SPKI_HEADER: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];
const ED25519_PKCS8_HEADER: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
/// The header of a P-256 SubjectPublicKeyInfo, for a key that is not ed25519.
const P256_SPKI_HEADER: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// Accepts whatever server answers. These tests are about the client's own signature.
#[derive(Debug)]
struct AcceptAnyServer {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyServer {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.3 only".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature_with_raw_key(
            message,
            &SubjectPublicKeyInfoDer::from(cert.as_ref()),
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// A TLS 1.3 client that presents `presented` as its key, signs with `signer`, and offers
/// `alpn`. The tests that vary one part of a client use `client_with`.
fn raw_client(presented: iroh::PublicKey, signer: &NodeKey, alpn: &[u8]) -> Arc<ClientConfig> {
    client_with(
        &[&TLS13],
        Some(raw_identity(ed25519_spki(presented), signer)),
        &[alpn],
    )
}

/// A TLS client that offers `versions` and `alpns`, and presents `identity` if it has one.
/// It accepts any server key: these tests are about the node's side of the handshake, not
/// the client's.
fn client_with(
    versions: &[&'static SupportedProtocolVersion],
    identity: Option<Arc<CertifiedKey>>,
    alpns: &[&[u8]],
) -> Arc<ClientConfig> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(versions)
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServer { provider }));
    let mut config = match identity {
        Some(identity) => builder
            .with_client_cert_resolver(Arc::new(AlwaysResolvesClientRawPublicKeys::new(identity))),
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = alpns.iter().map(|alpn| alpn.to_vec()).collect();
    Arc::new(config)
}

/// The identity a client presents: `presented` as its raw key, with `signer`'s ed25519 key
/// making the handshake signature.
fn raw_identity(presented: Vec<u8>, signer: &NodeKey) -> Arc<CertifiedKey> {
    let mut pkcs8 = ED25519_PKCS8_HEADER.to_vec();
    pkcs8.extend(hex::decode(signer.secret_hex()).unwrap());
    let key =
        aws_lc_rs::sign::any_supported_type(&PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8)))
            .unwrap();
    Arc::new(CertifiedKey::new(
        vec![CertificateDer::from(presented)],
        key,
    ))
}

/// The SubjectPublicKeyInfo that presents an ed25519 node key.
fn ed25519_spki(key: iroh::PublicKey) -> Vec<u8> {
    let mut spki = ED25519_SPKI_HEADER.to_vec();
    spki.extend_from_slice(key.as_bytes());
    spki
}

/// Starts a node on loopback, listening on each key in `listen` (`CLUSTER_TCP_BIND` or
/// `IROH_BIND`) at a free port, and publishes its heartbeat.
async fn start_node(
    database: &native::PostgresDatabase,
    root: &Path,
    name: &str,
    role: &str,
    listen: &[&str],
) -> cluster::Cluster {
    let settings = Settings::from_lookup(&|key| match key {
        "NODE_NAME" => Some(name.into()),
        key if listen.contains(&key) => Some("127.0.0.1:0".into()),
        _ => None,
    })
    .unwrap()
    .unwrap();
    let node = cluster::start(
        &database.pool,
        &root.join(format!("{name}-state")),
        &root.join(format!("{name}-repos")),
        &role.parse().unwrap(),
        settings,
    )
    .await
    .unwrap();
    node.publish().await.unwrap();
    node
}

async fn start_tcp(
    database: &native::PostgresDatabase,
    root: &Path,
    name: &str,
    role: &str,
) -> cluster::Cluster {
    start_node(database, root, name, role, &["CLUSTER_TCP_BIND"]).await
}

fn address_of(node: &cluster::Cluster) -> SocketAddr {
    node.tcp_addr().expect("the node listens on TCP")
}

/// A request that asks whether a repository exists. Any valid request serves these tests.
fn exists_request() -> rpc::Request {
    rpc::Request {
        repository: Uuid::new_v4(),
        operation: rpc::Operation::Exists,
        actor: None,
        ci_run: None,
    }
}

/// One exists-check request, sent as the RPC client sends it: a header, an empty body
/// frame, and the reply. Returns the reply frame, or the error if the node closed.
async fn exists(link: &mut Link) -> anyhow::Result<Vec<u8>> {
    let header = serde_json::to_vec(&exists_request()).unwrap();
    rpc::write_frame(&mut link.writer, &header, rpc::MAX_JSON).await?;
    rpc::write_frame(&mut link.writer, b"", rpc::MAX_BYTES).await?;
    link.finish().await?;
    let reply = rpc::read_frame(&mut link.reader, rpc::MAX_JSON).await?;
    rpc::read_frame(&mut link.reader, rpc::MAX_BYTES).await?;
    Ok(reply)
}

/// Connects as `config` and sends one exists-check request. A node that refuses the client
/// answers with an error. TLS 1.3 lets the client finish before the node checks its key, so
/// the refusal can show up on the first read rather than in `connect`.
async fn request_through(
    config: Arc<ClientConfig>,
    address: SocketAddr,
) -> anyhow::Result<Vec<u8>> {
    let socket = TcpStream::connect(address).await?;
    let mut stream = TlsConnector::from(config)
        .connect(ServerName::IpAddress(address.ip().into()), socket)
        .await?;
    let header = serde_json::to_vec(&exists_request())?;
    rpc::write_frame(&mut stream, &header, rpc::MAX_JSON).await?;
    rpc::write_frame(&mut stream, b"", rpc::MAX_BYTES).await?;
    rpc::read_frame(&mut stream, rpc::MAX_JSON).await
}

#[tokio::test]
async fn tcp_only_nodes_answer_repository_calls_without_an_iroh_socket() {
    let database = native::PostgresDatabase::new().await;
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let web = start_tcp(&database, root.path(), "web-1", "web").await;
    let storage = start_tcp(&database, root.path(), "store-1", "storage").await;
    assert!(web.endpoint().is_none() && storage.endpoint().is_none());
    let request = rpc::Request {
        repository: Uuid::new_v4(),
        operation: rpc::Operation::Exists,
        actor: None,
        ci_run: None,
    };
    web.context
        .call(storage.context.node_id, request, &[])
        .await
        .expect("the storage node answered over TCP");
}

#[tokio::test]
async fn pairing_over_tcp_admits_a_valid_ticket_and_refuses_a_bad_one() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let ticket = cluster::create_invite(
        &database.pool,
        core.public_key(),
        NodeAddress {
            tcp: vec![address_of(&core)],
            ..Default::default()
        },
        &InviteOptions {
            roles: "storage".parse().unwrap(),
            reusable: false,
            requires_approval: false,
            expires_in: Duration::from_secs(3600),
        },
    )
    .await
    .unwrap();
    let joining = NodeKey::generate();
    let membership = cluster::join(&ticket, &joining, &Relay::Off, "store-2", None)
        .await
        .expect("a valid ticket joins over TCP");
    assert_eq!(membership.status, "active");
    assert!(membership.newly_joined);

    let stranger = NodeKey::generate();
    let forged = JoinTicket::new(
        core.public_key(),
        NodeAddress {
            tcp: vec![address_of(&core)],
            ..Default::default()
        },
    );
    let error = cluster::join(&forged, &stranger, &Relay::Off, "intruder", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not valid"), "{error:#}");
    let nodes = db::cluster::list_nodes(&database.pool).await.unwrap();
    assert!(nodes.iter().all(|node| node.name != "intruder"));
}

#[tokio::test]
async fn unknown_keys_are_closed_on_rpc_and_events_before_any_answer() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let stranger = NodeKey::generate();
    for alpn in [ALPN_RPC, ALPN_EVENTS] {
        // The handshake succeeds for any well-formed key; membership is checked after it.
        let mut link = tcp::dial(&stranger, address_of(&core), core.public_key(), alpn)
            .await
            .expect("the handshake completes for any well-formed key");
        let answer = tokio::time::timeout(Duration::from_secs(5), async {
            let _ = rpc::write_frame(&mut link.writer, b"{}", rpc::MAX_JSON).await;
            rpc::read_frame(&mut link.reader, rpc::MAX_JSON).await
        })
        .await
        .expect("the node closes the connection promptly");
        assert!(
            answer.is_err(),
            "a non-member got an answer on {}",
            String::from_utf8_lossy(alpn)
        );
    }
}

#[tokio::test]
async fn revoked_member_is_refused_on_its_next_connection() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let member = NodeKey::generate();
    let declared =
        cluster::parse_declared_nodes(&format!("{}:web:web-2", member.public_hex())).unwrap();
    cluster::apply_declared_nodes(&database.pool, &declared)
        .await
        .unwrap();

    let mut link = tcp::dial(&member, address_of(&core), core.public_key(), ALPN_RPC)
        .await
        .unwrap();
    exists(&mut link)
        .await
        .expect("an active member gets an answer");

    cluster::revoke(&database.pool, db::cluster::Authority::Operator, "web-2")
        .await
        .unwrap();
    let mut link = tcp::dial(&member, address_of(&core), core.public_key(), ALPN_RPC)
        .await
        .unwrap();
    assert!(
        exists(&mut link).await.is_err(),
        "a revoked member got an answer"
    );
}

#[tokio::test]
async fn dialer_refuses_a_node_that_presents_another_key() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let node = start_tcp(&database, root.path(), "core-1", "storage").await;
    let impostor = NodeKey::generate().public_key();
    let error = tcp::dial(&NodeKey::generate(), address_of(&node), impostor, ALPN_RPC)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("pinned"), "{error:#}");
}

#[tokio::test]
async fn a_client_that_cannot_sign_for_a_members_key_is_refused() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    // The client presents the key of an active member, but signs with a key it holds.
    let config = raw_client(core.public_key(), &NodeKey::generate(), ALPN_RPC);
    let outcome = request_through(config, address_of(&core)).await;
    assert!(
        outcome.is_err(),
        "a key that cannot sign must not get an answer: {outcome:?}"
    );
}

#[tokio::test]
async fn an_unlisted_protocol_name_fails_the_handshake() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let address = address_of(&core);
    let client = NodeKey::generate();
    let config = raw_client(client.public_key(), &client, b"codeyard/other/1");
    let socket = TcpStream::connect(address).await.unwrap();
    // The node refuses the protocol name in its first flight, so `connect` itself fails.
    let handshake = TlsConnector::from(config)
        .connect(ServerName::IpAddress(address.ip().into()), socket)
        .await;
    assert!(
        handshake.is_err(),
        "a protocol name the node does not serve must fail the handshake"
    );
}

/// Sends one pairing request over a TCP link and reads the node's answer to its end.
async fn pairing_answer(link: &mut Link, request: &[u8]) -> std::io::Result<Vec<u8>> {
    link.writer.write_all(request).await?;
    link.finish().await?;
    let mut answer = Vec::new();
    link.reader.read_to_end(&mut answer).await?;
    Ok(answer)
}

/// Whether the node closes `link` without a word. A connection that holds a slot stays
/// open and silent, so this waits briefly and reports false for it.
async fn closed_by_node(link: &mut Link) -> bool {
    let mut byte = [0u8; 1];
    matches!(
        tokio::time::timeout(Duration::from_millis(100), link.reader.read(&mut byte)).await,
        Ok(Ok(0) | Err(_))
    )
}

/// Dials silent pairing peers until the node refuses one. The node refuses only when all
/// sixteen pairing slots are held, so the peers returned hold every slot. Nothing waits on
/// the node's own timing: the loop ends when the refusal arrives.
async fn hold_every_pairing_slot(address: SocketAddr, node: iroh::PublicKey) -> Vec<Link> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut held = Vec::new();
    loop {
        let mut peer = tcp::dial(&NodeKey::generate(), address, node, ALPN_PAIR)
            .await
            .expect("the handshake completes");
        if closed_by_node(&mut peer).await {
            assert!(
                held.len() >= 16,
                "the node refused a pairing peer after {} holders",
                held.len()
            );
            return held;
        }
        held.push(peer);
        assert!(
            tokio::time::Instant::now() < deadline,
            "the node never refused a pairing peer"
        );
    }
}

#[tokio::test]
async fn pairing_over_tcp_refuses_a_seventeenth_idle_peer_and_frees_slots_on_drop() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let address = address_of(&core);
    // Silent peers that finish the handshake hold the pairing slots, all sixteen of them.
    let idle = hold_every_pairing_slot(address, core.public_key()).await;
    // The seventeenth completes its handshake, and the node closes it without an answer.
    let mut refused = tcp::dial(&NodeKey::generate(), address, core.public_key(), ALPN_PAIR)
        .await
        .expect("the handshake completes");
    let answer = tokio::time::timeout(
        Duration::from_secs(5),
        pairing_answer(&mut refused, b"this is not json"),
    )
    .await
    .expect("the node closes the seventeenth connection at once");
    assert!(
        answer.is_err(),
        "a seventeenth idle pairing peer got an answer: {answer:?}"
    );
    // The idle peers hang up, which frees their slots: a new peer gets answers again.
    drop(idle);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let mut peer = tcp::dial(&NodeKey::generate(), address, core.public_key(), ALPN_PAIR)
            .await
            .expect("the handshake completes");
        let answer = pairing_answer(&mut peer, b"this is not json").await;
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
}

#[tokio::test]
async fn pairing_slots_are_shared_between_tcp_and_iroh() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_node(
        &database,
        root.path(),
        "core-1",
        "storage",
        &["CLUSTER_TCP_BIND", "IROH_BIND"],
    )
    .await;
    // Silent idle peers over TCP take every pairing slot the node has.
    let _idle = hold_every_pairing_slot(address_of(&core), core.public_key()).await;
    // A pairing peer over iroh is refused with the overload answer: the two listeners share
    // one handler, so the iroh listener has no slots of its own.
    let endpoint = core.endpoint().expect("the node listens on iroh");
    cluster::wait_for_address(endpoint).await;
    let client = cluster::bind_client(&Relay::Off, &NodeKey::generate())
        .await
        .unwrap();
    let connection = client
        .connect(endpoint.addr(), ALPN_PAIR)
        .await
        .expect("the iroh handshake completes");
    let closed = tokio::time::timeout(Duration::from_secs(5), connection.closed())
        .await
        .expect("the node refuses the iroh peer at once");
    match closed {
        iroh::endpoint::ConnectionError::ApplicationClosed(close) => {
            assert_eq!(u64::from(close.error_code), 503);
            assert_eq!(&close.reason[..], b"too many pairing requests");
        }
        other => panic!("the iroh peer was not refused for lack of slots: {other}"),
    }
    client.close().await;
}

#[tokio::test]
async fn garbage_and_stalled_clients_do_not_stop_the_listener() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let address = address_of(&core);

    // Bytes that are not TLS: the node closes the connection.
    let mut raw = TcpStream::connect(address).await.unwrap();
    raw.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    let mut sink = Vec::new();
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::io::AsyncReadExt::read_to_end(&mut raw, &mut sink),
    )
    .await;

    // A valid peer still gets through.
    tcp::dial(&NodeKey::generate(), address, core.public_key(), ALPN_PAIR)
        .await
        .expect("the listener still accepts");

    // Silent connections hold every handshake slot. The node accepts them in its own time,
    // so a valid peer is refused only once they all hold a slot. Dial until that refusal.
    let mut stalled = Vec::new();
    for _ in 0..64 {
        stalled.push(TcpStream::connect(address).await.unwrap());
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tcp::dial(&NodeKey::generate(), address, core.public_key(), ALPN_PAIR)
        .await
        .is_ok()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "a handshake over the limit was never closed"
        );
    }

    // The stalled handshakes end after five seconds and free their slots.
    while tcp::dial(&NodeKey::generate(), address, core.public_key(), ALPN_PAIR)
        .await
        .is_err()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the handshake slots were not released"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(stalled);
}

/// A loopback TCP address that nothing listens on.
fn unused_tcp_address() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// A loopback UDP address that nothing listens on.
fn unused_udp_address() -> SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

#[tokio::test]
async fn a_client_limited_to_tls_1_2_fails_the_handshake() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let address = address_of(&core);
    let client = NodeKey::generate();
    let config = client_with(
        &[&TLS12],
        Some(raw_identity(ed25519_spki(client.public_key()), &client)),
        &[ALPN_RPC],
    );
    let socket = TcpStream::connect(address).await.unwrap();
    // The node speaks TLS 1.3 only, so it refuses the TLS 1.2 hello in its first flight.
    let handshake = TlsConnector::from(config)
        .connect(ServerName::IpAddress(address.ip().into()), socket)
        .await;
    assert!(handshake.is_err(), "a TLS 1.2 client completed a handshake");
}

#[tokio::test]
async fn a_key_of_another_algorithm_is_refused() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    // A P-256 key presented with an ed25519 signature. The node checks the presented key first.
    let mut presented = P256_SPKI_HEADER.to_vec();
    presented.extend_from_slice(&[0x04; 65]);
    let client = NodeKey::generate();
    let config = client_with(
        &[&TLS13],
        Some(raw_identity(presented, &client)),
        &[ALPN_RPC],
    );
    let outcome = request_through(config, address_of(&core)).await;
    assert!(
        outcome.is_err(),
        "a key of another algorithm got an answer: {outcome:?}"
    );
}

#[tokio::test]
async fn a_client_without_a_key_gets_no_answer() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let config = client_with(&[&TLS13], None, &[ALPN_RPC]);
    let outcome = request_through(config, address_of(&core)).await;
    assert!(
        outcome.is_err(),
        "a client without a key got an answer: {outcome:?}"
    );
}

#[tokio::test]
async fn a_client_that_names_no_protocol_gets_no_answer() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let client = NodeKey::generate();
    let config = client_with(
        &[&TLS13],
        Some(raw_identity(ed25519_spki(client.public_key()), &client)),
        &[],
    );
    let outcome = request_through(config, address_of(&core)).await;
    assert!(
        outcome.is_err(),
        "a client that named no protocol got an answer: {outcome:?}"
    );
}

#[tokio::test]
async fn a_tcp_dial_opens_no_iroh_socket() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let dialer = IrohDialer::new(Relay::Off, NodeKey::generate());
    let address = NodeAddress {
        tcp: vec![address_of(&core)],
        ..Default::default()
    };
    let _link = cluster::dial_node(&dialer, core.public_key(), &address, ALPN_PAIR)
        .await
        .expect("the node answers over TCP");
    assert!(!dialer.is_open(), "a dial over TCP opened an iroh endpoint");
}

#[tokio::test]
async fn a_tcp_only_dialer_reaches_an_iroh_only_node_and_opens_iroh_for_it() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_node(&database, root.path(), "core-1", "storage", &["IROH_BIND"]).await;
    let endpoint = core.endpoint().expect("the node listens on iroh");
    cluster::wait_for_address(endpoint).await;
    let address = NodeAddress::of(&endpoint.addr());
    let dialer = IrohDialer::new(Relay::Off, NodeKey::generate());
    let _link = cluster::dial_node(&dialer, core.public_key(), &address, ALPN_PAIR)
        .await
        .expect("the node answers over iroh");
    assert!(
        dialer.is_open(),
        "the iroh endpoint did not open for an iroh-only node"
    );
}

#[tokio::test]
async fn a_dial_falls_back_from_tcp_to_iroh_and_then_uses_tcp_again() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_node(
        &database,
        root.path(),
        "core-1",
        "storage",
        &["CLUSTER_TCP_BIND", "IROH_BIND"],
    )
    .await;
    let endpoint = core.endpoint().expect("the node listens on iroh");
    cluster::wait_for_address(endpoint).await;
    let iroh_addrs = NodeAddress::of(&endpoint.addr()).addrs;
    let dialer = IrohDialer::new(Relay::Off, NodeKey::generate());
    // TCP refuses at once, so the dial moves on to iroh, which opens the endpoint.
    let fallback = NodeAddress {
        tcp: vec![unused_tcp_address()],
        addrs: iroh_addrs,
        ..Default::default()
    };
    let _link = cluster::dial_node(&dialer, core.public_key(), &fallback, ALPN_PAIR)
        .await
        .expect("the dial falls back to iroh");
    assert!(dialer.is_open(), "the dial did not reach iroh");
    // Once TCP answers, the next dial uses it first. Its iroh address is unused, so the dial
    // can succeed only over TCP.
    let back = NodeAddress {
        tcp: vec![address_of(&core)],
        addrs: vec![unused_udp_address()],
        ..Default::default()
    };
    let _link = cluster::dial_node(&dialer, core.public_key(), &back, ALPN_PAIR)
        .await
        .expect("the next dial goes back to TCP");
}

#[tokio::test]
async fn shutdown_closes_the_connections_the_listener_holds() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    // A pairing peer that connects and says nothing. Pairing would wait fifteen seconds.
    let mut idle = tcp::dial(
        &NodeKey::generate(),
        address_of(&core),
        core.public_key(),
        ALPN_PAIR,
    )
    .await
    .expect("the handshake completes");
    core.shutdown().await;
    let ended = tokio::time::timeout(
        Duration::from_secs(5),
        idle.reader.read_to_end(&mut Vec::new()),
    )
    .await;
    assert!(
        ended.is_ok(),
        "the node kept an idle connection open after shutdown"
    );
}

#[tokio::test]
async fn a_membership_check_that_stalls_keeps_its_handshake_slot() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let address = address_of(&core);
    let db::DbPool::Postgres(pool) = &database.pool else {
        panic!("the cluster tests run on PostgreSQL");
    };
    // Every membership check reads `nodes`, so with this lock held, each check waits.
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE nodes IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    // Peers finish the handshake and wait for the lock, each holding a slot. The node refuses
    // a handshake only when all sixty-four slots are held, so dial until one is refused.
    let mut waiting = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let attempt = tokio::time::timeout(
            Duration::from_secs(2),
            tcp::dial(&NodeKey::generate(), address, core.public_key(), ALPN_RPC),
        )
        .await
        .expect("the node answers each handshake attempt");
        match attempt {
            Ok(link) => waiting.push(link),
            Err(_) => break,
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a handshake over the limit got past the slot check"
        );
    }
    assert!(
        waiting.len() >= 64,
        "a handshake was refused after {} slots were held",
        waiting.len()
    );
    // Releasing the lock lets the waiting checks finish, and their slots come back.
    lock.rollback().await.unwrap();
    drop(waiting);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tcp::dial(&NodeKey::generate(), address, core.public_key(), ALPN_RPC)
        .await
        .is_err()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the handshake slots were not released"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A TCP address that accepts connections and never answers, so a dial to it runs out of
/// time. The accepted sockets stay open for as long as the test runs.
async fn silent_tcp_address() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    address
}

#[tokio::test]
async fn a_ci_claim_moves_on_from_a_tcp_address_that_never_answers() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let worker = start_tcp(&database, root.path(), "ci-1", "actions-worker").await;
    let queue = RunQueue::Remote {
        context: worker.context.clone(),
        peers: vec![cluster::CorePeer {
            public_key: core.public_key().to_string(),
            address: NodeAddress {
                tcp: vec![silent_tcp_address().await, address_of(&core)],
                ..Default::default()
            },
        }],
        preferred: Default::default(),
    };
    // With nothing queued, a claim is answered with `None`. Getting that answer proves the
    // request reached the core over the second address.
    let claimed = queue
        .claim("slot-1", CiExecutionMode::Local)
        .await
        .expect("the second TCP address answers");
    assert!(claimed.is_none());
}

#[tokio::test]
async fn a_ci_claim_reaches_the_core_over_iroh_after_a_tcp_address_never_answers() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_node(
        &database,
        root.path(),
        "core-1",
        "storage",
        &["CLUSTER_TCP_BIND", "IROH_BIND"],
    )
    .await;
    let endpoint = core.endpoint().expect("the node listens on iroh");
    cluster::wait_for_address(endpoint).await;
    let worker = start_tcp(&database, root.path(), "ci-1", "actions-worker").await;
    let queue = RunQueue::Remote {
        context: worker.context.clone(),
        peers: vec![cluster::CorePeer {
            public_key: core.public_key().to_string(),
            address: NodeAddress {
                tcp: vec![silent_tcp_address().await],
                addrs: NodeAddress::of(&endpoint.addr()).addrs,
                ..Default::default()
            },
        }],
        preferred: Default::default(),
    };
    let claimed = queue
        .claim("slot-1", CiExecutionMode::Local)
        .await
        .expect("iroh answers once TCP gives up");
    assert!(claimed.is_none());
}

#[tokio::test]
async fn shutdown_frees_the_tcp_port_before_it_returns() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_tcp(&database, root.path(), "core-1", "storage").await;
    let address = address_of(&core);
    core.shutdown().await;
    // Shutdown returns once the listening socket is closed, so the port binds at once.
    drop(
        std::net::TcpListener::bind(address)
            .expect("the port is still held after shutdown returned"),
    );
}

#[tokio::test]
async fn many_silent_tcp_addresses_share_one_dial_budget() {
    let mut tcp = Vec::new();
    for _ in 0..25 {
        tcp.push(silent_tcp_address().await);
    }
    let dialer = IrohDialer::new(Relay::Off, NodeKey::generate());
    let address = NodeAddress {
        tcp,
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let outcome = cluster::dial_node(
        &dialer,
        NodeKey::generate().public_key(),
        &address,
        ALPN_PAIR,
    )
    .await;
    let elapsed = started.elapsed();
    assert!(outcome.is_err(), "a dial to silent addresses connected");
    // The TCP addresses share two DIAL_TIMEOUTs. Trying each in turn would take over two
    // minutes.
    assert!(
        elapsed >= Duration::from_secs(9) && elapsed < Duration::from_secs(60),
        "the dial took {elapsed:?}"
    );
}

#[tokio::test]
async fn a_dial_reaches_iroh_after_many_silent_tcp_addresses() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let core = start_node(&database, root.path(), "core-1", "storage", &["IROH_BIND"]).await;
    let endpoint = core.endpoint().expect("the node listens on iroh");
    cluster::wait_for_address(endpoint).await;
    let mut tcp = Vec::new();
    for _ in 0..25 {
        tcp.push(silent_tcp_address().await);
    }
    let dialer = IrohDialer::new(Relay::Off, NodeKey::generate());
    let address = NodeAddress {
        tcp,
        addrs: NodeAddress::of(&endpoint.addr()).addrs,
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let _link = cluster::dial_node(&dialer, core.public_key(), &address, ALPN_PAIR)
        .await
        .expect("iroh answers once the TCP budget is spent");
    let elapsed = started.elapsed();
    // TCP is tried first, so the dial waits out the shared budget before iroh answers.
    assert!(
        elapsed >= Duration::from_secs(9) && elapsed < Duration::from_secs(60),
        "the dial took {elapsed:?}"
    );
}
