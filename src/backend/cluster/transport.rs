//! The iroh endpoint: protocol names, relay choice, binding, and the rule that only
//! active cluster members may connect.
use super::{identity::NodeKey, link::Link, pairing::PairingHandler, tcp};
use crate::backend::db::{self, DbPool};
use anyhow::{anyhow, bail, Context, Result};
use iroh::{
    endpoint::{presets, AfterHandshakeOutcome, Connection, EndpointHooks, QuicTransportConfig},
    protocol::{Router, RouterBuilder},
    Endpoint, EndpointAddr, PublicKey, RelayMode, RelayUrl, TransportAddr,
};
use serde::{Deserialize, Serialize};
use std::{fmt, net::SocketAddr, str::FromStr, sync::Arc};
use tokio::sync::OnceCell;
use tracing::warn;

/// One-time handshake that adds a new node. The only protocol open to keys that are
/// not (yet) members, and it can do nothing but pair.
pub const ALPN_PAIR: &[u8] = b"codeyard/pair/1";
/// Request and response calls between nodes. Members only.
pub const ALPN_RPC: &[u8] = b"codeyard/rpc/3";
/// CI job claims and log upload for workers without a database. Members only.
pub const ALPN_EVENTS: &[u8] = b"codeyard/events/2";

/// Close code for a peer that is not an active member.
pub const CLOSE_NOT_A_MEMBER: u32 = 403;
/// Close code for "cannot decide or cannot serve right now".
pub const CLOSE_UNAVAILABLE: u32 = 503;

/// Whether to use relay servers, which carry traffic when two nodes cannot reach
/// each other directly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Relay {
    /// Direct connections only. This is the default.
    #[default]
    Off,
    /// The public relays run by the iroh project (`IROH_RELAY=n0`).
    Public,
    /// Relays the operator runs (`IROH_RELAY=<url>[,<url>]`).
    Custom(Vec<RelayUrl>),
}

impl Relay {
    /// Reads the value of `IROH_RELAY`. Unset or empty means no relay.
    pub fn parse(value: Option<&str>) -> Result<Relay> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(Relay::Off);
        };
        if value.eq_ignore_ascii_case("n0") {
            return Ok(Relay::Public);
        }
        let urls = value
            .split(',')
            .map(|url| RelayUrl::from_str(url.trim()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                anyhow!("IROH_RELAY must be `n0` or one or more relay URLs separated by commas")
            })?;
        Ok(Relay::Custom(urls))
    }

    pub fn is_off(&self) -> bool {
        matches!(self, Relay::Off)
    }

    fn mode(&self) -> RelayMode {
        match self {
            Relay::Off => RelayMode::Disabled,
            Relay::Public => RelayMode::Default,
            Relay::Custom(urls) => RelayMode::custom(urls.iter().cloned()),
        }
    }
}

/// How to reach a node. This is our own shape, not iroh's, because it is stored in
/// tickets that may sit in a secret store for months and in `nodes.addr_hint`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAddress {
    /// Direct UDP addresses.
    #[serde(default)]
    pub addrs: Vec<SocketAddr>,
    /// Relay server URLs.
    #[serde(default)]
    pub relays: Vec<String>,
    /// TCP addresses of the node's listener (`CLUSTER_TCP_BIND`).
    #[serde(default)]
    pub tcp: Vec<SocketAddr>,
}

impl NodeAddress {
    pub fn of(address: &EndpointAddr) -> NodeAddress {
        NodeAddress {
            addrs: address.ip_addrs().copied().collect(),
            relays: address.relay_urls().map(ToString::to_string).collect(),
            tcp: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.addrs.is_empty() && self.relays.is_empty() && self.tcp.is_empty()
    }

    pub fn endpoint_addr(&self, node: PublicKey) -> Result<EndpointAddr> {
        let relays = self
            .relays
            .iter()
            .map(|url| RelayUrl::from_str(url).map(TransportAddr::Relay))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| anyhow!("the address contains an invalid relay URL"))?;
        Ok(EndpointAddr::from_parts(
            node,
            self.addrs
                .iter()
                .copied()
                .map(TransportAddr::Ip)
                .chain(relays),
        ))
    }
}

/// Where this node listens.
#[derive(Clone, Debug)]
pub struct Listen {
    pub bind: SocketAddr,
    pub relay: Relay,
    /// Addresses to tell peers about when the bound address is not the one they
    /// should dial (NAT, container port mapping).
    pub advertise: Vec<SocketAddr>,
}

/// Lets only active cluster members through, on every protocol except pairing.
/// The member list is read from the database for each new connection, so a revoke
/// takes effect on the next connection attempt.
struct MembersOnly {
    pool: DbPool,
}

impl fmt::Debug for MembersOnly {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MembersOnly")
            .finish_non_exhaustive()
    }
}

impl EndpointHooks for MembersOnly {
    async fn after_handshake<'a>(&'a self, connection: &'a Connection) -> AfterHandshakeOutcome {
        // Only connections that peers open to us are checked; our own are not.
        if connection.side().is_client() || connection.alpn() == ALPN_PAIR {
            return AfterHandshakeOutcome::Accept;
        }
        match active_member(&self.pool, connection.remote_id()).await {
            Ok(true) => AfterHandshakeOutcome::Accept,
            Ok(false) => AfterHandshakeOutcome::Reject {
                error_code: CLOSE_NOT_A_MEMBER.into(),
                reason: b"not an active cluster member".to_vec(),
            },
            Err(error) => {
                // Without an answer the safe choice is to refuse.
                warn!(%error, "could not check cluster membership");
                AfterHandshakeOutcome::Reject {
                    error_code: CLOSE_UNAVAILABLE.into(),
                    reason: b"membership check unavailable".to_vec(),
                }
            }
        }
    }
}

/// Used at handshake and immediately before every repository request.
pub(crate) async fn active_member(pool: &DbPool, peer: PublicKey) -> Result<bool> {
    db::cluster::is_active_node(pool, &peer.to_string()).await
}

/// iroh's connections wait 30 s for a silent peer. Three missed 5 s heartbeats is enough
/// here: a crashed storage node then ends the Git sessions that use it promptly.
fn quic_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .max_idle_timeout(Some(
            std::time::Duration::from_secs(15)
                .try_into()
                .expect("15 s is a valid idle timeout"),
        ))
        .build()
}

/// Opens the UDP socket for the cluster. Peers that are not active members are
/// refused on every protocol except pairing.
pub async fn bind(listen: &Listen, key: &NodeKey, pool: DbPool) -> Result<Endpoint> {
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(key.iroh_secret())
        .relay_mode(listen.relay.mode())
        .transport_config(quic_config())
        // Only the configured socket, not the extra random-port sockets iroh adds.
        .clear_ip_transports()
        .bind_addr(listen.bind)
        .map_err(|error| anyhow!("IROH_BIND {} is not usable: {error}", listen.bind))?
        .hooks(MembersOnly { pool });
    for address in &listen.advertise {
        builder = builder.external_addr(*address);
    }
    builder
        .bind()
        .await
        .with_context(|| format!("cannot open the cluster UDP socket on {}", listen.bind))
}

/// An endpoint for dialing out only: it listens on a random port and accepts nothing.
pub async fn bind_client(relay: &Relay, key: &NodeKey) -> Result<Endpoint> {
    Endpoint::builder(presets::Minimal)
        .secret_key(key.iroh_secret())
        .relay_mode(relay.mode())
        .transport_config(quic_config())
        .bind()
        .await
        .context("cannot open a socket to reach the cluster")
}

/// The endpoint a node dials iroh peers from. It is opened the first time an iroh peer
/// needs it, so a node that dials only over TCP never opens a UDP socket.
#[derive(Clone)]
pub struct IrohDialer {
    relay: Relay,
    key: NodeKey,
    opened: Arc<OnceCell<Endpoint>>,
}

impl IrohDialer {
    /// A dialer that opens its endpoint on first use.
    pub fn new(relay: Relay, key: NodeKey) -> Self {
        Self {
            relay,
            key,
            opened: Arc::new(OnceCell::new()),
        }
    }

    /// A dialer that dials from an endpoint the node already has.
    pub fn with_endpoint(relay: Relay, key: NodeKey, endpoint: Endpoint) -> Self {
        Self {
            relay,
            key,
            opened: Arc::new(OnceCell::new_with(Some(endpoint))),
        }
    }

    pub fn key(&self) -> &NodeKey {
        &self.key
    }

    /// The endpoint, opened now if no iroh peer has needed it yet.
    pub async fn endpoint(&self) -> Result<&Endpoint> {
        // Boxed: binding is a large future, and every dial that may need iroh awaits this.
        // Kept on the heap, it does not enlarge those callers' futures.
        self.opened
            .get_or_try_init(|| Box::pin(bind_client(&self.relay, &self.key)))
            .await
    }

    /// Whether the endpoint has been opened. A node that has only dialed over TCP has none.
    pub fn is_open(&self) -> bool {
        self.opened.initialized()
    }

    /// Closes the endpoint if it was opened.
    pub async fn close(&self) {
        if let Some(endpoint) = self.opened.get() {
            endpoint.close().await;
        }
    }
}

/// How long one attempt to reach a node may take, over TCP or over iroh.
pub const DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long all TCP attempts to one node may take together: two `DIAL_TIMEOUT`s.
const TCP_DIAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Opens a request stream to a node. The TCP addresses are tried first, in order. They share
/// `TCP_DIAL_BUDGET`: each attempt takes at most what is left of it, and once that is spent no
/// more TCP addresses are tried. iroh comes next, and only when the node also has iroh addresses
/// or relays. Opening the iroh endpoint and dialing over iroh share one `DIAL_TIMEOUT`. A call
/// takes at most 15 seconds, so callers must not wrap it in a shorter timeout: that would cut
/// off the later addresses. Closing the returned link ends the connection.
pub async fn dial_node(
    iroh: &IrohDialer,
    node: PublicKey,
    address: &NodeAddress,
    alpn: &[u8],
) -> Result<Link> {
    let budget = tokio::time::Instant::now() + TCP_DIAL_BUDGET;
    let mut failure = anyhow!("the node has no address to dial");
    for tcp_address in &address.tcp {
        let left = budget.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(
            left.min(DIAL_TIMEOUT),
            Box::pin(tcp::dial(iroh.key(), *tcp_address, node, alpn)),
        )
        .await
        {
            Ok(Ok(link)) => return Ok(link),
            Ok(Err(error)) => failure = error,
            Err(_) => failure = anyhow!("timed out reaching {tcp_address} over TCP"),
        }
    }
    if address.addrs.is_empty() && address.relays.is_empty() {
        return Err(failure);
    }
    let target = address.endpoint_addr(node)?;
    // Opening the endpoint can take time too, so it runs under the same timeout as the dial.
    tokio::time::timeout(DIAL_TIMEOUT, async move {
        let endpoint = iroh
            .endpoint()
            .await
            .context("cannot open an iroh endpoint to reach the node")?;
        Box::pin(dial(endpoint, target, alpn)).await
    })
    .await
    .map_err(|_| anyhow!("timed out reaching the node over iroh"))?
}

/// Opens a request stream to `address` over QUIC.
async fn dial(endpoint: &Endpoint, address: EndpointAddr, alpn: &[u8]) -> Result<Link> {
    let connection = endpoint.connect(address, alpn).await?;
    let (send, recv) = connection.open_bi().await?;
    Ok(Link::quic(
        connection.remote_id(),
        alpn,
        recv,
        send,
        connection,
    ))
}

/// The next request stream a peer opens on an accepted connection. `None` once the
/// connection has ended.
pub async fn accept_stream(connection: &Connection) -> Option<Link> {
    let (send, recv) = connection.accept_bi().await.ok()?;
    Some(Link::quic(
        connection.remote_id(),
        connection.alpn(),
        recv,
        send,
        connection.clone(),
    ))
}

/// A router that already accepts pairing through `pairing`, so the caller can share that
/// handler with the TCP listener. Add other protocols with `.accept(..)`, then `.spawn()`.
pub fn router(endpoint: Endpoint, pairing: PairingHandler) -> RouterBuilder {
    Router::builder(endpoint).accept(ALPN_PAIR, pairing)
}

/// Resolves `host:port` entries (as set in `IROH_ADVERTISE`) to socket addresses.
pub async fn resolve_advertised(entries: &[String]) -> Result<Vec<SocketAddr>> {
    let mut resolved = Vec::new();
    for entry in entries {
        let mut found = tokio::net::lookup_host(entry.as_str())
            .await
            .with_context(|| format!("cannot resolve IROH_ADVERTISE entry `{entry}`"))?
            .peekable();
        if found.peek().is_none() {
            bail!("IROH_ADVERTISE entry `{entry}` has no address");
        }
        resolved.extend(found);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_defaults_to_off_and_parses_the_documented_forms() {
        assert_eq!(Relay::parse(None).unwrap(), Relay::Off);
        assert_eq!(Relay::parse(Some("  ")).unwrap(), Relay::Off);
        assert_eq!(Relay::parse(Some("n0")).unwrap(), Relay::Public);
        assert_eq!(Relay::parse(Some("N0")).unwrap(), Relay::Public);
        match Relay::parse(Some(
            "https://relay.example.com, https://relay2.example.com",
        ))
        .unwrap()
        {
            Relay::Custom(urls) => assert_eq!(urls.len(), 2),
            other => panic!("unexpected {other:?}"),
        }
        assert!(Relay::parse(Some("relay.example.com")).is_err());
        assert!(Relay::parse(Some("n0,https://relay.example.com")).is_err());
        assert!(Relay::Off.is_off() && !Relay::Public.is_off());
    }

    #[test]
    fn node_addresses_survive_storage_and_reject_bad_relays() {
        let key = NodeKey::generate().public_key();
        let address = NodeAddress {
            addrs: vec![
                "10.0.0.5:7444".parse().unwrap(),
                "[::1]:7444".parse().unwrap(),
            ],
            relays: vec!["https://relay.example.com/".into()],
            tcp: vec!["10.0.0.5:7445".parse().unwrap()],
        };
        let json = serde_json::to_string(&address).unwrap();
        let back: NodeAddress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, address);
        let endpoint_address = back.endpoint_addr(key).unwrap();
        assert_eq!(endpoint_address.id, key);
        assert_eq!(NodeAddress::of(&endpoint_address).addrs.len(), 2);
        assert_eq!(NodeAddress::of(&endpoint_address).relays.len(), 1);
        let bad = NodeAddress {
            relays: vec!["not a url".into()],
            ..Default::default()
        };
        assert!(bad.endpoint_addr(key).is_err());
        assert!(NodeAddress::default().is_empty());
        assert!(!NodeAddress {
            tcp: vec!["10.0.0.5:7445".parse().unwrap()],
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn node_addresses_written_before_tcp_still_load() {
        let old: NodeAddress = serde_json::from_str(
            r#"{"addrs":["10.0.0.5:7444"],"relays":["https://relay.example.com/"]}"#,
        )
        .unwrap();
        assert_eq!(
            old.addrs,
            vec!["10.0.0.5:7444".parse::<SocketAddr>().unwrap()]
        );
        assert!(old.tcp.is_empty());
        let empty: NodeAddress = serde_json::from_str("{}").unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn protocol_names_are_distinct_and_versioned() {
        assert_eq!(ALPN_PAIR, b"codeyard/pair/1");
        assert_eq!(ALPN_RPC, b"codeyard/rpc/3");
        assert_eq!(ALPN_EVENTS, b"codeyard/events/2");
        let names = [ALPN_PAIR, ALPN_RPC, ALPN_EVENTS];
        for (index, name) in names.iter().enumerate() {
            assert!(names[index + 1..].iter().all(|other| other != name));
        }
    }
}
