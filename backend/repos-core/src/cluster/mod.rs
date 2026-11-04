//! Cluster membership: node keys, pairing, and the listeners that let paired nodes reach
//! each other, over iroh or over TCP with TLS. A plain single node never calls into this module: it
//! starts only when cluster settings are present in the environment.
use crate::context::NodeContext;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use codeyard_cluster::pairing::apply_declared_nodes;
use codeyard_cluster::pairing::join;
use codeyard_cluster::pairing::JoinTicket;
use codeyard_cluster::pairing::PairingHandler;
use codeyard_cluster::transport::bind;
use codeyard_cluster::transport::resolve_advertised;
use codeyard_cluster::transport::IrohDialer;
use codeyard_cluster::transport::Listen;
use codeyard_cluster::transport::NodeAddress;
use codeyard_cluster::transport::ALPN_EVENTS;
use codeyard_cluster::transport::ALPN_RPC;
use codeyard_db_core::models::ClusterNode;
use codeyard_db_core::DbPool;
use codeyard_model::node_roles::NodeRole;
use codeyard_model::node_roles::NodeRoles;
use codeyard_support::node_identity::{load_existing, load_or_create, NodeKey};
use iroh::Endpoint;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::task::AbortOnDropHandle;
use tracing::info;
use tracing::warn;
pub mod ci;
pub mod placement;
pub mod replicas;
pub mod rpc;
pub mod tcp;
pub mod worker;

/// A running cluster node: the listener, and once published, the heartbeat. Dropping it
/// stops both abruptly; [`Cluster::shutdown`] withdraws the node, closes connections cleanly
/// and releases the node's identity and storage locks.
pub struct Cluster {
    /// The iroh listener. `None` on a node that runs without iroh.
    router: Option<iroh::protocol::Router>,
    /// The TCP listener, when the node listens on TCP.
    tcp_listener: Option<crate::cluster::tcp::Listener>,
    heartbeat: std::sync::Mutex<Option<AbortOnDropHandle<()>>>,
    pool: DbPool,
    storage_path: Option<PathBuf>,
    /// The TCP addresses this node reports to peers.
    tcp_advertised: Vec<SocketAddr>,
    /// Admits the repository sessions this node serves. `None` on a node without storage.
    sessions: Option<crate::cluster::rpc::SessionGate>,
    events: crate::cluster::ci::CiHandler,
    pub context: NodeContext,
    /// Held until [`Cluster::shutdown`], so a node with the same identity can start after it.
    identity_lock: std::sync::Mutex<Option<codeyard_db_cluster::cluster::ServingLock>>,
    repository_lock: std::sync::Mutex<Option<std::fs::File>>,
}

impl Cluster {
    /// The iroh endpoint, or `None` on a node that runs without iroh.
    pub fn endpoint(&self) -> Option<&Endpoint> {
        self.router.as_ref().map(|router| router.endpoint())
    }

    /// The address this node listens on for TCP, if it does.
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp_listener
            .as_ref()
            .map(crate::cluster::tcp::Listener::addr)
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
            crate::cluster::replicas::reconcile(
                &self.pool,
                self.context.node_id,
                &self.context.repos_path,
            )
            .await?;
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
        self.events.start_claiming();
        let heartbeat = tokio::spawn(heartbeat(
            self.pool.clone(),
            self.public_key(),
            self.endpoint().cloned(),
            self.tcp_advertised.clone(),
            self.storage_path.clone(),
        ));
        drop(
            self.heartbeat
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .replace(AbortOnDropHandle::new(heartbeat)),
        );
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
        self.events.close();
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
        let clear = codeyard_db_cluster::cluster::clear_node_heartbeat(&self.pool, &key);
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
        self.events.drain().await;
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
        drop(
            self.repository_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        );
        let identity = self
            .identity_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(identity) = identity {
            identity.release().await;
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
    let identity_lock = codeyard_db_cluster::cluster::serving_lock(pool, &key.public_hex()).await?;
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
            codeyard_db_cluster::cluster::register_own_node(
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
    anyhow::ensure!(
        roles.names().iter().all(|role| node.roles.contains(role)),
        "requested service roles exceed enrollment for '{}'; update CLUSTER_NODES with the allowed roles and restart a core node before starting these services",
        node.name
    );
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
    let storage = roles.contains(NodeRole::Storage);
    ensure!(
        storage || !codeyard_db_cluster::cluster::has_storage_assignments(pool, node.id).await?,
        "move this node's primaries and replica assignments before removing its storage service"
    );
    let repos_path = if storage {
        std::fs::create_dir_all(repos_path)?;
        repos_path.canonicalize()?
    } else {
        std::path::absolute(repos_path)?
    };
    let repository_lock = if storage {
        Some(codeyard_support::locks::exclusive_file(
            &repos_path.join("storage.lock"),
        )?)
    } else {
        None
    };
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
        ci_replicas: Default::default(),
    };
    let storage_path = roles.contains(NodeRole::Storage).then_some(repos_path);
    let rpc = storage_path
        .is_some()
        .then(|| crate::cluster::rpc::RpcHandler::new(context.clone()));
    let sessions = rpc.as_ref().map(crate::cluster::rpc::RpcHandler::sessions);
    let ci_config = std::sync::Arc::new(codeyard_ci::config::CiConfig::from_env()?);
    let queue = crate::ci::queue::RunQueue::Database {
        pool: pool.clone(),
        config: ci_config,
        secrets: codeyard_support::secrets::SecretEncryptor::configured_from_env()?
            .map(std::sync::Arc::new),
        context: Some(context.clone()),
        notifications: std::sync::Arc::new(
            codeyard_notifications::broadcast::NotificationBroadcaster::new(pool),
        ),
    };
    let events = crate::cluster::ci::CiHandler::new(context.clone(), queue);
    // Claims wait until publish has recorded this node's address; see `Cluster::publish`.
    events.stop_claiming();
    // One pairing handler serves both listeners, so they share its slots.
    let pairing = PairingHandler::new(pool.clone());
    let router = match endpoint {
        Some(endpoint) => {
            let mut router = codeyard_cluster::transport::router(endpoint, pairing.clone());
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
            let services = crate::cluster::tcp::Services {
                pool: pool.clone(),
                pairing,
                rpc,
                events: events.clone(),
            };
            let listener = crate::cluster::tcp::bind(bind_address, &key, services).await?;
            let advertised = if settings.tcp_advertise.is_empty() {
                default_tcp_advertise(listener.addr())
            } else {
                resolve_advertised(&settings.tcp_advertise).await?
            };
            (Some(listener), advertised)
        }
        None => (None, Vec::new()),
    };
    ensure!(
        !storage || router.is_some() || !tcp_advertised.is_empty(),
        "TCP-only storage needs CLUSTER_TCP_ADVERTISE when CLUSTER_TCP_BIND is unspecified"
    );
    Ok(Cluster {
        identity_lock: std::sync::Mutex::new(Some(identity_lock)),
        repository_lock: std::sync::Mutex::new(repository_lock),
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
        ensure!(
            !codeyard_db_cluster::cluster::cluster_enabled(pool).await?,
            "this database is in cluster mode; set the CLUSTER_* settings to run repository commands"
        );
        return Ok(None);
    };
    require_postgres(pool)?;
    codeyard_support::fs::durable_dir(state_dir)?;
    let identity_lock = codeyard_support::locks::exclusive_file(&state_dir.join("serve.lock"))?;
    let key = load_existing(state_dir)?
        .ok_or_else(|| anyhow!("this node has not joined the cluster; start serve first"))?;
    let node = codeyard_db_cluster::cluster::get_node_by_public_key(pool, &key.public_hex())
        .await?
        .filter(|node| node.status == "active")
        .ok_or_else(|| anyhow!("this node is not an active cluster member"))?;
    ensure!(
        node.roles.iter().any(|role| role == "web" || role == "ssh"),
        "repository operator commands require an active web or ssh node identity"
    );
    Ok(Some(NodeContext {
        pool: Some(pool.clone()),
        iroh: IrohDialer::new(settings.relay.clone(), key.clone())
            .with_identity_lock(identity_lock),
        key,
        node_id: node.id,
        repos_path: std::path::absolute(repos_path)?,
        peers: Default::default(),
        unreachable: Default::default(),
        ci_replicas: Default::default(),
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
    if let Some(node) =
        codeyard_db_cluster::cluster::get_node_by_public_key(pool, &public_key).await?
    {
        return Ok(node);
    }
    let membership = join(ticket, key, &settings.relay, &settings.name, Some(roles)).await?;
    info!(name = %membership.name, status = %membership.status, "joined the cluster");
    codeyard_db_cluster::cluster::get_node_by_public_key(pool, &public_key)
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

/// Free space, in bytes, on the filesystem that holds `path`.
pub fn disk_free(path: &Path) -> Result<i64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // statvfs reads the filesystem containing the repository root.
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and outlives the call, and statvfs writes only `stats`.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: statvfs succeeded, so it filled `stats`.
    let stats = unsafe { stats.assume_init() };
    Ok(i64::try_from(
        stats.f_bavail.saturating_mul(stats.f_frsize),
    )?)
}

/// Records a sign of life and the addresses this node can be reached at. Returns
/// false if the node has no row.
pub async fn report_alive(
    pool: &DbPool,
    key: iroh::PublicKey,
    hint: NodeAddress,
    storage_path: Option<&Path>,
) -> Result<bool> {
    ensure!(
        storage_path.is_none() || !hint.is_empty(),
        "storage node has no advertised address; configure CLUSTER_TCP_ADVERTISE or IROH_ADVERTISE"
    );
    let hint = serde_json::to_string(&hint)?;
    let free = if let Some(path) = storage_path {
        static PROBES: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
            std::sync::OnceLock::new();
        let permit = PROBES
            .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)))
            .clone()
            .try_acquire_owned()
            .context("disk probe is still running")?;
        let path = path.to_owned();
        Some(
            tokio::time::timeout(
                Duration::from_secs(3),
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    disk_free(&path)
                }),
            )
            .await
            .context("disk probe timed out")???,
        )
    } else {
        None
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        codeyard_db_cluster::cluster::record_node_heartbeat(
            pool,
            &key.to_string(),
            VERSION,
            &hint,
            free,
        ),
    )
    .await
    .context("heartbeat database update timed out")?
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
    // A fork inserted while a family is being locked leaves that family untouched. Refresh
    // the directory snapshot to include it; never move the source without its current forks.
    for _ in 0..3 {
        let local: Vec<_> = codeyard_db_cluster::cluster::list_unplaced_repositories(pool)
            .await?
            .into_iter()
            .filter(|(_, disk_path)| {
                crate::cluster::rpc::repository::repository_uuid(disk_path).is_ok_and(|uuid| {
                    crate::repositories::store::primary_path(repos_path, uuid).exists()
                })
            })
            .map(|(id, _)| id)
            .collect();
        let mut placed = std::collections::HashSet::new();
        let mut attempted = std::collections::HashSet::new();
        for &id in &local {
            if attempted.insert(id) {
                attempted.extend(
                    codeyard_db_cluster::cluster::list_fork_family(pool, id)
                        .await?
                        .into_iter()
                        .map(|(member, _, _)| member),
                );
                placed.extend(
                    codeyard_db_cluster::cluster::place_fork_family(pool, id, node_id, &local)
                        .await?,
                );
            }
        }
        adopted += placed.len();
    }
    Ok(adopted)
}

use codeyard_cluster::settings::{
    default_tcp_advertise, require_postgres, Settings, HEARTBEAT_INTERVAL, VERSION,
    WITHDRAW_TIMEOUT,
};
