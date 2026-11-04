use crate::pairing::{normalize_name, parse_declared_nodes, DeclaredNode, JoinTicket};
use crate::transport::Relay;
use anyhow::{anyhow, bail, ensure, Context, Result};
use codeyard_db_core::DbPool;
use codeyard_support::node_identity::parse_public_key;
use std::{net::SocketAddr, time::Duration};
/// How often a running node records that it is alive and where it can be reached.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// How long shutdown waits for the database to clear this node's heartbeat. A row lock held
/// elsewhere must not stall the drain. If the clear is skipped, the node stops counting as
/// online about a minute after its last heartbeat.
pub const WITHDRAW_TIMEOUT: Duration = Duration::from_secs(3);

const DEFAULT_BIND: &str = "0.0.0.0:7444";
pub const NEEDS_POSTGRES: &str = "Clustering needs PostgreSQL. This installation uses SQLite, which supports a single node only. Point DATABASE_URL at a PostgreSQL database to use cluster features.";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
pub fn hostname() -> Result<String> {
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

/// The TCP address peers dial when `CLUSTER_TCP_ADVERTISE` is not set: the bound address,
/// unless it is unspecified, which no peer can dial.
pub fn default_tcp_advertise(bound: SocketAddr) -> Vec<SocketAddr> {
    if bound.ip().is_unspecified() {
        warn!(%bound, "CLUSTER_TCP_BIND is unspecified and CLUSTER_TCP_ADVERTISE is not set; peers cannot be told where to dial this node");
        Vec::new()
    } else {
        vec![bound]
    }
}
