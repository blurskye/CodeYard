//! Cluster membership: node keys, pairing, and the listeners that let paired nodes reach
//! each other, over iroh or over TCP with TLS. A plain single node never calls into this module: it
//! starts only when cluster settings are present in the environment.
pub mod ci;
pub mod context;
mod identity;
pub mod link;
mod pairing;
pub mod placement;
pub mod replicas;
pub mod rpc;
pub mod tcp;
mod tls;
mod transport;
pub(crate) mod worker;
pub use context::NodeContext;
pub use worker::{worker_context, CorePeer};

pub use identity::{load_existing, load_or_create, parse_public_key, NodeKey};
pub use pairing::{
    apply_declared_nodes, approve, create_invite, hash_secret, join, normalize_name,
    parse_declared_nodes, parse_duration, revoke, DeclaredNode, DeclaredSummary, InviteOptions,
    JoinTicket, Membership, PairingHandler, MAX_INVITE_LIFETIME,
};
pub use transport::{
    bind, bind_client, dial_node, resolve_advertised, router, IrohDialer, Listen, NodeAddress,
    Relay, ALPN_EVENTS, ALPN_PAIR, ALPN_RPC, CLOSE_NOT_A_MEMBER, CLOSE_UNAVAILABLE,
};

use crate::backend::{
    db::{self, models::ClusterNode, DbPool},
    node::{NodeRole, NodeRoles},
};
use anyhow::{anyhow, bail, ensure, Context, Result};

use iroh::Endpoint;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// How often a running node records that it is alive and where it can be reached.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// How long shutdown waits for the database to clear this node's heartbeat. A row lock held
/// elsewhere must not stall the drain. If the clear is skipped, the node stops counting as
/// online about a minute after its last heartbeat.
pub(crate) const WITHDRAW_TIMEOUT: Duration = Duration::from_secs(3);

const DEFAULT_BIND: &str = "0.0.0.0:7444";
pub(crate) const NEEDS_POSTGRES: &str = "Clustering needs PostgreSQL. This installation uses SQLite, which supports a single node only. Point DATABASE_URL at a PostgreSQL database to use cluster features.";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Variables that switch cluster mode on. With none of them set, `serve` runs as a
/// plain single node and opens no cluster socket.
const SWITCHES: [&str; 11] = [
    "CLUSTER_PEERS",
    "NODE_KEY",
    "NODE_KEY_FILE",
    "CLUSTER_NODES",
    "CLUSTER_NODES_FILE",
    "CLUSTER_JOIN_TOKEN",
    "CLUSTER_TCP_BIND",
    "CLUSTER_TCP_ADVERTISE",
    "IROH_BIND",
    "IROH_RELAY",
    "IROH_ADVERTISE",
];

/// Clustering shares one PostgreSQL database between machines; SQLite cannot.
pub fn require_postgres(pool: &DbPool) -> Result<()> {
    match pool {
        DbPool::Postgres(_) => Ok(()),
        DbPool::Sqlite(_) => bail!(NEEDS_POSTGRES),
    }
}

/// Cluster settings read from the environment.
#[derive(Debug)]
pub struct Settings {
    /// The UDP address for iroh. `None` when the node runs without iroh.
    pub bind: Option<SocketAddr>,
    pub relay: Relay,
    /// `host:port` entries to advertise instead of the bound address.
    pub advertise: Vec<String>,
    /// The TCP address for node connections, if the node listens on TCP.
    pub tcp_bind: Option<SocketAddr>,
    /// `host:port` entries peers dial over TCP. Empty means the bound TCP address, unless
    /// that address is unspecified.
    pub tcp_advertise: Vec<String>,
    pub name: String,
    pub declared: Vec<DeclaredNode>,
    pub join_ticket: Option<JoinTicket>,
    /// `(public key, host:port)` from CLUSTER_PEERS. A host that starts with `tcp://` is
    /// dialed over TCP.
    pub peers: Vec<(String, String)>,
}

impl Settings {
    /// `None` means no cluster setting is present: a plain single node.
    pub fn from_env() -> Result<Option<Settings>> {
        Self::from_lookup(&|name| std::env::var(name).ok())
    }

    /// Like [`Settings::from_env`] with the variables supplied by `lookup`.
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Option<Settings>> {
        // Compose files pass unset variables through as empty strings.
        let value = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        if SWITCHES.iter().all(|name| value(name).is_none()) {
            return Ok(None);
        }
        let tcp_bind = value("CLUSTER_TCP_BIND")
            .map(|text| {
                text.parse::<SocketAddr>()
                    .map_err(|_| anyhow!("CLUSTER_TCP_BIND must be an address like 0.0.0.0:7445"))
            })
            .transpose()?;
        // iroh runs unless this node listens on TCP and sets no IROH_* variable.
        let iroh_set = ["IROH_BIND", "IROH_RELAY", "IROH_ADVERTISE"]
            .into_iter()
            .any(|name| value(name).is_some());
        let bind = if iroh_set || tcp_bind.is_none() {
            let bind = value("IROH_BIND").unwrap_or_else(|| DEFAULT_BIND.to_owned());
            let bind = bind
                .parse::<SocketAddr>()
                .map_err(|_| anyhow!("IROH_BIND must be an address like {DEFAULT_BIND}"))?;
            Some(bind)
        } else {
            None
        };
        let relay = Relay::parse(value("IROH_RELAY").as_deref())?;
        let advertise = value("IROH_ADVERTISE")
            .map(|list| {
                list.split(',')
                    .map(|entry| entry.trim().to_owned())
                    .filter(|entry| !entry.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let tcp_advertise: Vec<String> = value("CLUSTER_TCP_ADVERTISE")
            .map(|list| {
                list.split(',')
                    .map(|entry| entry.trim().to_owned())
                    .filter(|entry| !entry.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        ensure!(
            tcp_bind.is_some() || tcp_advertise.is_empty(),
            "CLUSTER_TCP_ADVERTISE needs CLUSTER_TCP_BIND"
        );
        let name = match value("NODE_NAME") {
            Some(name) => name,
            None => hostname()?,
        };
        let name = normalize_name(&name).context("the node name (NODE_NAME, or the host name)")?;
        let declared = match (value("CLUSTER_NODES"), value("CLUSTER_NODES_FILE")) {
            (Some(_), Some(_)) => bail!("set CLUSTER_NODES or CLUSTER_NODES_FILE, not both"),
            (Some(text), None) => parse_declared_nodes(&text).context("CLUSTER_NODES")?,
            (None, Some(path)) => {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("cannot read CLUSTER_NODES_FILE {path}"))?;
                parse_declared_nodes(&text).with_context(|| format!("CLUSTER_NODES_FILE {path}"))?
            }
            (None, None) => Vec::new(),
        };
        let join_ticket = value("CLUSTER_JOIN_TOKEN")
            .map(|text| JoinTicket::decode(&text).context("CLUSTER_JOIN_TOKEN"))
            .transpose()?;
        let peers = value("CLUSTER_PEERS")
            .map(|list| {
                list.split(',')
                    .map(|entry| {
                        let (key, host) = entry
                            .trim()
                            .split_once('@')
                            .context("CLUSTER_PEERS entries must be <pubkey>@<host:port>")?;
                        let public_key = parse_public_key(key)?.to_string();
                        let target = host.strip_prefix("tcp://").unwrap_or(host);
                        ensure!(!target.is_empty(), "CLUSTER_PEERS needs a host:port");
                        Ok((public_key, host.to_owned()))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Some(Settings {
            bind,
            relay,
            advertise,
            tcp_bind,
            tcp_advertise,
            name,
            declared,
            join_ticket,
            peers,
        }))
    }
}

/// The machine's host name, used as the node name when `NODE_NAME` is not set.
pub(crate) fn hostname() -> Result<String> {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|name| !name.is_empty())
        })
        .ok_or_else(|| anyhow!("cannot tell this machine's name; set NODE_NAME"))
}

/// A running cluster node: the listener, and once published, the heartbeat. Dropping it
/// stops both abruptly; [`Cluster::shutdown`] withdraws the node and closes connections
/// cleanly.
pub struct Cluster {
    /// The iroh listener. `None` on a node that runs without iroh.
    router: Option<iroh::protocol::Router>,
    /// The TCP listener, when the node listens on TCP.
    tcp_listener: Option<tcp::Listener>,
    heartbeat: std::sync::Mutex<Option<JoinHandle<()>>>,
    pool: DbPool,
    storage_path: Option<PathBuf>,
    /// The TCP addresses this node reports to peers.
    tcp_advertised: Vec<SocketAddr>,
    /// Admits the repository sessions this node serves. `None` on a node without storage.
    sessions: Option<rpc::SessionGate>,
    events: ci::CiHandler,
    pub context: NodeContext,
}

/// The TCP address peers dial when `CLUSTER_TCP_ADVERTISE` is not set: the bound address,
/// unless it is unspecified, which no peer can dial.
fn default_tcp_advertise(bound: SocketAddr) -> Vec<SocketAddr> {
    if bound.ip().is_unspecified() {
        warn!(%bound, "CLUSTER_TCP_BIND is unspecified and CLUSTER_TCP_ADVERTISE is not set; peers cannot be told where to dial this node");
        Vec::new()
    } else {
        vec![bound]
    }
}

impl Cluster {
    /// The iroh endpoint, or `None` on a node that runs without iroh.
    pub fn endpoint(&self) -> Option<&Endpoint> {
        self.router.as_ref().map(|router| router.endpoint())
    }

    /// The address this node listens on for TCP, if it does.
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp_listener.as_ref().map(tcp::Listener::addr)
    }

    /// This node's key: its identity in `nodes`.
    pub fn public_key(&self) -> iroh::PublicKey {
        self.context.key.public_key()
    }

    /// The addresses this node reports to peers: iroh's, when it runs iroh, and the TCP
    /// addresses it advertises.
    fn address_hint(&self) -> NodeAddress {
        address_hint(self.endpoint(), &self.tcp_advertised)
    }

    /// Publishes this node as live: records where it can be reached and starts the
    /// heartbeat. Placement and the admin page count the node as live from here until
    /// [`Cluster::withdraw`]. Call it once the node's other services are set up.
    pub async fn publish(&self) -> Result<()> {
        if self.storage_path.is_some() {
            replicas::reconcile(&self.pool, self.context.node_id, &self.context.repos_path).await?;
        }
        if let Some(endpoint) = self.endpoint() {
            wait_for_address(endpoint).await;
        }
        report_alive(
            &self.pool,
            self.public_key(),
            self.address_hint(),
            self.storage_path.as_deref(),
        )
        .await?;
        let heartbeat = tokio::spawn(heartbeat(
            self.pool.clone(),
            self.public_key(),
            self.endpoint().cloned(),
            self.tcp_advertised.clone(),
            self.storage_path.clone(),
        ));
        let previous = self
            .heartbeat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(heartbeat);
        if let Some(previous) = previous {
            previous.abort();
        }
        info!(
            key = %self.public_key(),
            listening = ?self.endpoint().map(|endpoint| endpoint.bound_sockets()),
            tcp = ?self.tcp_addr(),
            "cluster node ready"
        );
        Ok(())
    }

    /// Stops publishing this node as live: the heartbeat ends and `last_seen` is cleared,
    /// so placement skips the node at once. Shutdown calls this before the node drains.
    /// The clear is best effort: after `WITHDRAW_TIMEOUT` shutdown goes on without it.
    /// Repository sessions from other nodes are refused from here on.
    pub async fn withdraw(&self) {
        self.events.stop_claiming();
        if let Some(sessions) = &self.sessions {
            sessions.close();
        }
        let heartbeat = self
            .heartbeat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(heartbeat) = heartbeat {
            heartbeat.abort();
            let _ = heartbeat.await;
        }
        let key = self.public_key().to_string();
        let clear = db::cluster::clear_node_heartbeat(&self.pool, &key);
        match tokio::time::timeout(WITHDRAW_TIMEOUT, clear).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!(%error, "could not clear this node's heartbeat"),
            Err(_) => warn!("could not clear this node's heartbeat within {WITHDRAW_TIMEOUT:?}"),
        }
    }

    /// Waits for the repository sessions this node is serving to finish. Call it after
    /// [`Cluster::withdraw`]; the caller bounds the wait. Sessions still running after
    /// [`Cluster::shutdown`] are ended with their connections.
    pub async fn drain_sessions(&self) {
        if let Some(sessions) = &self.sessions {
            sessions.drain().await;
        }
    }

    pub async fn shutdown(&self) {
        self.withdraw().await;
        if let Some(listener) = &self.tcp_listener {
            listener.stop().await;
        }
        if let Some(router) = &self.router {
            if let Err(error) = router.shutdown().await {
                warn!(%error, "the cluster listener did not stop cleanly");
            }
        }
        self.context.iroh.close().await;
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        if let Some(heartbeat) = self
            .heartbeat
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            heartbeat.abort();
        }
    }
}

/// Brings this node into the cluster and starts listening: loads the node key,
/// applies the declared member list, registers or joins, and opens the socket. The
/// node is not published as live yet; call [`Cluster::publish`] once its other
/// services are running.
pub async fn start(
    pool: &DbPool,
    state_dir: &Path,
    repos_path: &Path,
    roles: &NodeRoles,
    settings: Settings,
) -> Result<Cluster> {
    require_postgres(pool)?;
    let key = load_or_create(state_dir)?;
    if !settings.declared.is_empty() {
        let summary = apply_declared_nodes(pool, &settings.declared).await?;
        info!(
            applied = summary.applied,
            "declared cluster members applied"
        );
        for name in &summary.kept_revoked {
            warn!(%name, "node is listed in CLUSTER_NODES but revoked; it stays revoked until `cluster approve`");
        }
    }
    let node = match &settings.join_ticket {
        Some(ticket) => join_if_new(pool, &key, ticket, &settings, roles).await?,
        None => {
            // A node that holds the database credentials registers itself.
            db::cluster::register_own_node(
                pool,
                &key.public_hex(),
                &settings.name,
                &roles.names(),
                VERSION,
            )
            .await
            .context("cannot register this node (if its name is taken, set NODE_NAME to another)")?
        }
    };
    match node.status.as_str() {
        "revoked" => bail!(
            "this node ('{}') was revoked from the cluster; run `codeyard cluster approve {}` to let it back in, or remove its cluster settings",
            node.name,
            node.name
        ),
        "pending" => warn!(
            name = %node.name,
            "this node waits for approval; an administrator runs `codeyard cluster approve {}`",
            node.name
        ),
        _ => {}
    }
    // iroh listens only when the settings give it an address (see `Settings::bind`).
    let endpoint = match settings.bind {
        Some(bind_address) => {
            let listen = Listen {
                bind: bind_address,
                relay: settings.relay.clone(),
                advertise: resolve_advertised(&settings.advertise).await?,
            };
            Some(bind(&listen, &key, pool.clone()).await?)
        }
        None => None,
    };
    std::fs::create_dir_all(repos_path)?;
    let repos_path = repos_path.canonicalize()?;
    let iroh = match &endpoint {
        Some(endpoint) => {
            IrohDialer::with_endpoint(settings.relay.clone(), key.clone(), endpoint.clone())
        }
        None => IrohDialer::new(settings.relay.clone(), key.clone()),
    };
    let context = NodeContext {
        pool: Some(pool.clone()),
        iroh,
        key: key.clone(),
        node_id: node.id,
        repos_path: repos_path.clone(),
        peers: Default::default(),
        unreachable: Default::default(),
    };
    let storage_path = roles.contains(NodeRole::Storage).then_some(repos_path);
    let rpc = storage_path
        .is_some()
        .then(|| rpc::RpcHandler::new(context.clone()));
    let sessions = rpc.as_ref().map(rpc::RpcHandler::sessions);
    let ci_config = std::sync::Arc::new(crate::backend::ci::CiConfig::from_env()?);
    let queue = crate::backend::ci::queue::RunQueue::Database {
        pool: pool.clone(),
        config: ci_config,
        secrets: crate::backend::crypto::SecretEncryptor::configured_from_env()?
            .map(std::sync::Arc::new),
        context: Some(context.clone()),
        notifications: std::sync::Arc::new(
            crate::backend::notifications::NotificationBroadcaster::new(pool),
        ),
    };
    let events = ci::CiHandler::new(context.clone(), queue);
    // One pairing handler serves both listeners, so they share its slots.
    let pairing = PairingHandler::new(pool.clone());
    let router = match endpoint {
        Some(endpoint) => {
            let mut router = transport::router(endpoint, pairing.clone());
            if let Some(rpc) = &rpc {
                router = router.accept(ALPN_RPC, rpc.clone());
            }
            Some(router.accept(ALPN_EVENTS, events.clone()).spawn())
        }
        None => None,
    };
    // The TCP listener serves the same handlers as the iroh router, behind TLS.
    let (tcp_listener, tcp_advertised) = match settings.tcp_bind {
        Some(bind_address) => {
            let services = tcp::Services {
                pool: pool.clone(),
                pairing,
                rpc,
                events: events.clone(),
            };
            let listener = tcp::bind(bind_address, &key, services).await?;
            let advertised = if settings.tcp_advertise.is_empty() {
                default_tcp_advertise(listener.addr())
            } else {
                resolve_advertised(&settings.tcp_advertise).await?
            };
            (Some(listener), advertised)
        }
        None => (None, Vec::new()),
    };
    Ok(Cluster {
        router,
        tcp_listener,
        heartbeat: std::sync::Mutex::new(None),
        pool: pool.clone(),
        storage_path,
        tcp_advertised,
        sessions,
        events,
        context,
    })
}

/// Operator commands reuse this node's identity but dial from an ephemeral socket.
pub async fn client_context(
    pool: &DbPool,
    state_dir: &Path,
    repos_path: &Path,
) -> Result<Option<NodeContext>> {
    let Some(settings) = Settings::from_env()? else {
        return Ok(None);
    };
    require_postgres(pool)?;
    let key = load_existing(state_dir)?
        .ok_or_else(|| anyhow!("this node has not joined the cluster; start serve first"))?;
    let node = db::cluster::get_node_by_public_key(pool, &key.public_hex())
        .await?
        .filter(|node| node.status == "active")
        .ok_or_else(|| anyhow!("this node is not an active cluster member"))?;
    Ok(Some(NodeContext {
        pool: Some(pool.clone()),
        iroh: IrohDialer::new(settings.relay.clone(), key.clone()),
        key,
        node_id: node.id,
        repos_path: std::path::absolute(repos_path)?,
        peers: Default::default(),
        unreachable: Default::default(),
    }))
}

/// Pairs through the ticket's core node unless this key is already a member, in which
/// case the saved key is reused and the token is not needed again.
async fn join_if_new(
    pool: &DbPool,
    key: &NodeKey,
    ticket: &JoinTicket,
    settings: &Settings,
    roles: &NodeRoles,
) -> Result<ClusterNode> {
    let public_key = key.public_hex();
    if let Some(node) = db::cluster::get_node_by_public_key(pool, &public_key).await? {
        return Ok(node);
    }
    let membership = join(ticket, key, &settings.relay, &settings.name, Some(roles)).await?;
    info!(name = %membership.name, status = %membership.status, "joined the cluster");
    db::cluster::get_node_by_public_key(pool, &public_key)
        .await?
        .ok_or_else(|| {
            anyhow!("the cluster accepted this node but it is not in this database; CLUSTER_JOIN_TOKEN must come from a node that uses the same DATABASE_URL")
        })
}

/// Gives a new endpoint a moment to learn its addresses, so the first report is useful.
/// If it learns none, the heartbeat reports them once it does.
pub async fn wait_for_address(endpoint: &Endpoint) {
    for _ in 0..100 {
        if !NodeAddress::of(&endpoint.addr()).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    warn!("this node has no address to report yet");
}

/// Records a sign of life and the addresses this node can be reached at. Returns
/// false if the node has no row.
fn disk_free(path: &Path) -> Result<i64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // statvfs reads the filesystem containing the repository root.
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stats = unsafe { stats.assume_init() };
    Ok(i64::try_from(
        stats.f_bavail.saturating_mul(stats.f_frsize),
    )?)
}

pub async fn report_alive(
    pool: &DbPool,
    key: iroh::PublicKey,
    hint: NodeAddress,
    storage_path: Option<&Path>,
) -> Result<bool> {
    let hint = serde_json::to_string(&hint)?;
    let free = storage_path.map(disk_free).transpose()?;
    db::cluster::record_node_heartbeat(pool, &key.to_string(), VERSION, &hint, free).await
}

/// The addresses a node reports for peers to dial: iroh's, when it runs iroh, and the TCP
/// addresses it advertises.
fn address_hint(endpoint: Option<&Endpoint>, tcp: &[SocketAddr]) -> NodeAddress {
    let mut address = endpoint
        .map(|endpoint| NodeAddress::of(&endpoint.addr()))
        .unwrap_or_default();
    address.tcp = tcp.to_vec();
    address
}

async fn heartbeat(
    pool: DbPool,
    key: iroh::PublicKey,
    endpoint: Option<Endpoint>,
    tcp: Vec<SocketAddr>,
    storage_path: Option<PathBuf>,
) {
    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick is immediate and `start` has just reported.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        // Read again each time: iroh may have learned new addresses since the last report.
        let hint = address_hint(endpoint.as_ref(), &tcp);
        if let Err(error) = report_alive(&pool, key, hint, storage_path.as_deref()).await {
            warn!(%error, "could not record this node's heartbeat");
        }
    }
}

/// Gives `node_id` the repositories that have no storage node and whose directory is
/// in `repos_path`. Returns how many it took.
pub async fn adopt_repositories(pool: &DbPool, node_id: i64, repos_path: &Path) -> Result<usize> {
    let mut adopted = 0;
    for (id, disk_path) in db::cluster::list_unplaced_repositories(pool).await? {
        let here = rpc::repository_uuid(&disk_path)
            .is_ok_and(|uuid| repos_path.join(format!("{uuid}.git")).exists());
        if here && db::cluster::place_repository(pool, id, node_id).await? {
            adopted += 1;
        }
    }
    Ok(adopted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, &str)]) -> Result<Option<Settings>> {
        Settings::from_lookup(&|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn no_cluster_setting_means_a_plain_single_node() {
        assert!(settings(&[]).unwrap().is_none());
        // Names and empty values do not switch anything on by themselves.
        assert!(settings(&[("NODE_NAME", "box")]).unwrap().is_none());
        for name in SWITCHES {
            assert!(
                settings(&[(name, ""), ("NODE_NAME", "box")])
                    .unwrap()
                    .is_none(),
                "{name}"
            );
            assert!(settings(&[(name, "  ")]).unwrap().is_none(), "{name}");
        }
    }

    #[test]
    fn defaults_are_direct_only_on_the_documented_port() {
        let key = NodeKey::generate().public_hex();
        let found = settings(&[
            ("NODE_NAME", "Core-1"),
            ("CLUSTER_NODES", &format!("{key}:web:core-1")),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(found.bind, Some("0.0.0.0:7444".parse().unwrap()));
        assert!(found.relay.is_off());
        assert!(found.advertise.is_empty());
        assert_eq!(found.name, "core-1");
        assert_eq!(found.declared.len(), 1);
        assert!(found.join_ticket.is_none());
    }

    #[test]
    fn each_setting_is_read_and_checked() {
        let found = settings(&[
            ("NODE_NAME", "w1"),
            ("IROH_BIND", "127.0.0.1:9000"),
            ("IROH_RELAY", "n0"),
            ("IROH_ADVERTISE", "node.example.com:7444, 203.0.113.9:7444"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(found.bind.unwrap().port(), 9000);
        assert_eq!(found.relay, Relay::Public);
        assert_eq!(
            found.advertise,
            ["node.example.com:7444", "203.0.113.9:7444"]
        );

        assert!(settings(&[("NODE_NAME", "w1"), ("IROH_BIND", "not an address")]).is_err());
        assert!(settings(&[("NODE_NAME", "w1"), ("IROH_RELAY", "nonsense")]).is_err());
        assert!(settings(&[("NODE_NAME", "Bad Name!"), ("IROH_BIND", "127.0.0.1:1")]).is_err());
        assert!(settings(&[("NODE_NAME", "w1"), ("CLUSTER_JOIN_TOKEN", "garbage")]).is_err());
        assert!(settings(&[("NODE_NAME", "w1"), ("CLUSTER_NODES", "garbage")]).is_err());
        let both = settings(&[
            ("NODE_NAME", "w1"),
            ("CLUSTER_NODES", ""),
            ("CLUSTER_NODES_FILE", "/nonexistent"),
        ]);
        assert!(both.is_err(), "an unreadable file is an error, not silence");
    }

    #[test]
    fn core_peers_are_ordered_and_checked() {
        let first = NodeKey::generate().public_hex();
        let second = NodeKey::generate().public_hex();
        let found = settings(&[(
            "CLUSTER_PEERS",
            &format!("{first}@core.example:7444,{second}@[::1]:7444"),
        )])
        .unwrap()
        .unwrap();
        assert_eq!(
            found.peers,
            [
                (first, "core.example:7444".into()),
                (second, "[::1]:7444".into())
            ]
        );
        assert!(settings(&[("CLUSTER_PEERS", "bad@host:7444")]).is_err());
        assert!(settings(&[("CLUSTER_PEERS", "missing-at")]).is_err());
    }

    #[test]
    fn tcp_listener_runs_without_iroh_unless_an_iroh_setting_is_present() {
        let tcp_only = settings(&[("NODE_NAME", "w1"), ("CLUSTER_TCP_BIND", "0.0.0.0:7445")])
            .unwrap()
            .unwrap();
        assert_eq!(
            tcp_only.bind, None,
            "no UDP socket without an IROH_* setting"
        );
        assert_eq!(tcp_only.tcp_bind, Some("0.0.0.0:7445".parse().unwrap()));
        let both = settings(&[
            ("NODE_NAME", "w1"),
            ("CLUSTER_TCP_BIND", "0.0.0.0:7445"),
            ("IROH_BIND", "127.0.0.1:9000"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(both.bind.unwrap().port(), 9000);
        assert!(settings(&[("NODE_NAME", "w1"), ("CLUSTER_TCP_BIND", "not an address")]).is_err());
        assert!(
            settings(&[
                ("NODE_NAME", "w1"),
                ("CLUSTER_TCP_ADVERTISE", "core.example:7445"),
            ])
            .is_err(),
            "an advertised TCP address needs a TCP listener"
        );
    }

    #[test]
    fn tcp_peers_are_marked_and_the_advertised_address_defaults_to_the_bound_one() {
        let first = NodeKey::generate().public_hex();
        let second = NodeKey::generate().public_hex();
        let found = settings(&[
            ("NODE_NAME", "w1"),
            ("CLUSTER_TCP_BIND", "10.0.0.5:7445"),
            (
                "CLUSTER_PEERS",
                &format!("{first}@tcp://core.example:7445,{second}@core.example:7444"),
            ),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            found.peers,
            [
                (first, "tcp://core.example:7445".into()),
                (second, "core.example:7444".into())
            ]
        );
        // Start fills in the bound address once the listener is up.
        assert!(found.tcp_advertise.is_empty());
        assert!(settings(&[("NODE_NAME", "w1"), ("CLUSTER_PEERS", "bad@tcp://x:1")]).is_err());
    }

    #[test]
    fn an_unspecified_tcp_bind_advertises_nothing_by_default() {
        assert_eq!(
            default_tcp_advertise("10.0.0.5:7445".parse().unwrap()),
            vec!["10.0.0.5:7445".parse::<SocketAddr>().unwrap()]
        );
        assert!(default_tcp_advertise("0.0.0.0:7445".parse().unwrap()).is_empty());
        assert!(default_tcp_advertise("[::]:7445".parse().unwrap()).is_empty());
    }

    #[test]
    fn nodes_file_and_join_ticket_are_read() {
        let directory = tempfile::tempdir().unwrap();
        let key = NodeKey::generate().public_hex();
        let file = directory.path().join("nodes");
        std::fs::write(&file, format!("# members\n{key}:storage:store-1\n")).unwrap();
        let ticket = JoinTicket::new(
            NodeKey::generate().public_key(),
            NodeAddress {
                addrs: vec!["127.0.0.1:7444".parse().unwrap()],
                ..Default::default()
            },
        );
        let found = settings(&[
            ("NODE_NAME", "w1"),
            ("CLUSTER_NODES_FILE", file.to_str().unwrap()),
            ("CLUSTER_JOIN_TOKEN", &ticket.encode()),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(found.declared[0].name, "store-1");
        assert_eq!(found.join_ticket.unwrap(), ticket);
        let twice = settings(&[
            ("NODE_NAME", "w1"),
            ("CLUSTER_NODES", &format!("{key}:web:a")),
            ("CLUSTER_NODES_FILE", file.to_str().unwrap()),
        ]);
        assert!(twice.is_err());
    }
}
