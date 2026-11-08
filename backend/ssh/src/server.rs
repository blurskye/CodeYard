use crate::handler::AuthOutcome;
use crate::handler::GitSshHandler;
use crate::rate_limit::client_key;
use crate::rate_limit::RateLimiter;
use anyhow::Context;
use anyhow::Result;
use codeyard_ci::runs::CiRuns;
use codeyard_database::DbPool;
use rand::rngs::OsRng;
use rand::RngCore;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::PrivateKey;
use russh::server::Config;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use tokio::io::AsyncReadExt as _;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::info;
use tracing::warn;

/// A new Ed25519 host key from the operating system's random source.
fn new_host_key() -> PrivateKey {
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    PrivateKey::from(Ed25519Keypair::from_seed(&seed))
}

fn open_host_key_file(path: &str) -> Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let path = std::path::absolute(path)?;
    let uid = codeyard_git_storage::repositories::git_sessions::private_files::effective_uid();
    codeyard_git_storage::repositories::git_sessions::private_files::ancestors(
        path.parent().context("Host key parent required")?,
        uid,
    )?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    anyhow::ensure!(
        meta.is_file() && meta.nlink() == 1 && (meta.uid() == uid || meta.uid() == 0),
        "Unsafe host key file"
    );
    if meta.mode() & 0o077 != 0 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn write_host_key_file(path: &str, key_pem: &str) -> Result<()> {
    let path_ref = Path::new(path);
    if let Some(parent) = path_ref.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create host key directory {:?}", parent))?;
        }
    }

    let path_ref = std::path::absolute(path_ref)?;
    let parent = path_ref.parent().context("Host key parent required")?;
    let uid = codeyard_git_storage::repositories::git_sessions::private_files::effective_uid();
    codeyard_git_storage::repositories::git_sessions::private_files::ancestors(parent, uid)?;
    match open_host_key_file(path) {
        Ok(_) => {}
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(key_pem.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path_ref)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

const TRUSTED_PROXY_IPS_ENV: &str = "SSH_TRUSTED_PROXY_IPS";
const PROXY_PROTOCOL_PREFIX: &[u8] = b"PROXY ";
const MAX_PROXY_HEADER_LEN: usize = 108;
const PROXY_HEADER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
/// Unauthenticated connections each hold a task and a socket, so they are capped
/// overall and per client address until they authenticate.
const MAX_PREAUTH_CONNECTIONS: usize = 128;
const MAX_PREAUTH_CONNECTIONS_PER_IP: usize = 16;
/// Every open connection, authenticated or not, holds sockets and tasks until it ends.
const MAX_CONNECTIONS: usize = 512;
/// Time allowed to authenticate. Each rejected key waits `auth_rejection_time` (3 s)
/// and russh allows `max_auth_attempts` (10) rejections, so a client holding the most
/// keys still has time to try them all.
const LOGIN_GRACE_TIME: std::time::Duration = std::time::Duration::from_secs(60);
/// Pause after a failed accept, such as running out of file descriptors.
const ACCEPT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// A client of a dual-stack listener can appear as an IPv4-mapped IPv6 address, which must match
/// the IPv4 address configured for it.
fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(address) => address.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

/// Shuts the socket down when dropped. A cancelled connection drops its russh session task
/// without ending that task, so this is the handle that closes the connection.
pub(crate) struct ShutdownOnDrop(pub(crate) std::net::TcpStream);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

fn configured_trusted_proxy_ips() -> Result<HashSet<IpAddr>> {
    let Some(value) = std::env::var_os(TRUSTED_PROXY_IPS_ENV) else {
        return Ok(HashSet::new());
    };
    let value = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("{TRUSTED_PROXY_IPS_ENV} must be valid UTF-8"))?;
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<IpAddr>()
                .map(canonical_ip)
                .with_context(|| format!("Invalid IP address in {TRUSTED_PROXY_IPS_ENV}: {value}"))
        })
        .collect()
}

async fn starts_with_proxy_header(stream: &TcpStream) -> Result<bool> {
    let mut prefix = [0; 6];
    let detected = tokio::time::timeout(PROXY_HEADER_TIMEOUT, async {
        loop {
            let read = stream.peek(&mut prefix).await?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed before SSH identification",
                ));
            }
            if read >= PROXY_PROTOCOL_PREFIX.len() {
                return Ok(prefix.as_slice() == PROXY_PROTOCOL_PREFIX);
            }
            if !PROXY_PROTOCOL_PREFIX.starts_with(&prefix[..read]) {
                return Ok(false);
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await;

    match detected {
        Ok(result) => result.map_err(Into::into),
        // A normal SSH client may wait for the server identification. In that
        // case no forwarding metadata was presented, so keep the TCP peer.
        Err(_) => Ok(false),
    }
}

async fn read_proxy_header(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut line = Vec::with_capacity(64);
    while line.len() < MAX_PROXY_HEADER_LEN {
        line.push(stream.read_u8().await?);
        if line.ends_with(b"\r\n") {
            return Ok(line);
        }
    }
    anyhow::bail!("PROXY protocol header is too long")
}

fn proxy_header_source(line: &[u8]) -> Result<Option<SocketAddr>> {
    let line = std::str::from_utf8(line).context("PROXY protocol header is not UTF-8")?;
    let line = line
        .strip_suffix("\r\n")
        .context("PROXY protocol header has no line ending")?;
    let mut fields = line.split_ascii_whitespace();
    anyhow::ensure!(
        fields.next() == Some("PROXY"),
        "Invalid PROXY protocol header"
    );
    let protocol = fields
        .next()
        .context("PROXY protocol header has no address family")?;
    // PROXY v1 says a receiver ignores the rest of an UNKNOWN header.
    if protocol == "UNKNOWN" {
        return Ok(None);
    }
    anyhow::ensure!(
        protocol == "TCP4" || protocol == "TCP6",
        "Unsupported PROXY protocol family"
    );

    let source_ip: IpAddr = fields
        .next()
        .context("PROXY protocol header has no source address")?
        .parse()
        .context("Invalid PROXY protocol source address")?;
    let destination_ip: IpAddr = fields
        .next()
        .context("PROXY protocol header has no destination address")?
        .parse()
        .context("Invalid PROXY protocol destination address")?;
    let source_port: u16 = fields
        .next()
        .context("PROXY protocol header has no source port")?
        .parse()
        .context("Invalid PROXY protocol source port")?;
    let _: u16 = fields
        .next()
        .context("PROXY protocol header has no destination port")?
        .parse()
        .context("Invalid PROXY protocol destination port")?;
    anyhow::ensure!(fields.next().is_none(), "Unexpected PROXY protocol fields");
    anyhow::ensure!(
        matches!(
            (protocol, source_ip, destination_ip),
            ("TCP4", IpAddr::V4(_), IpAddr::V4(_)) | ("TCP6", IpAddr::V6(_), IpAddr::V6(_))
        ),
        "PROXY protocol address family mismatch"
    );

    Ok(Some(SocketAddr::new(source_ip, source_port)))
}

async fn connection_source(
    stream: &mut TcpStream,
    peer: SocketAddr,
    trusted_proxy_ips: &HashSet<IpAddr>,
) -> Result<SocketAddr> {
    if !trusted_proxy_ips.contains(&canonical_ip(peer.ip()))
        || !starts_with_proxy_header(stream).await?
    {
        return Ok(peer);
    }

    let line = tokio::time::timeout(PROXY_HEADER_TIMEOUT, read_proxy_header(stream))
        .await
        .context("Timed out reading PROXY protocol header")??;
    Ok(proxy_header_source(&line)?.unwrap_or(peer))
}

/// Counts unauthenticated connections, overall and per client address. A slot is
/// held until its connection authenticates or ends.
#[derive(Clone, Default)]
struct PreAuthAdmission(Arc<Mutex<PreAuthCounts>>);

#[derive(Default)]
struct PreAuthCounts {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

impl PreAuthAdmission {
    fn reserve(&self) -> Option<PreAuthSlot> {
        let mut counts = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if counts.total >= MAX_PREAUTH_CONNECTIONS {
            return None;
        }
        counts.total += 1;
        Some(PreAuthSlot {
            admission: self.clone(),
            ip: None,
            global: true,
        })
    }
}

struct PreAuthSlot {
    admission: PreAuthAdmission,
    ip: Option<IpAddr>,
    global: bool,
}

impl PreAuthSlot {
    fn release_global(&mut self) {
        if self.global {
            self.admission
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .total -= 1;
            self.global = false;
        }
    }

    fn for_ip(mut self, ip: IpAddr) -> Option<Self> {
        let ip = client_key(ip);
        {
            let mut counts = self
                .admission
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let count = counts.per_ip.entry(ip).or_default();
            if *count >= MAX_PREAUTH_CONNECTIONS_PER_IP {
                return None;
            }
            *count += 1;
            self.ip = Some(ip);
        }
        Some(self)
    }
}

impl Drop for PreAuthSlot {
    fn drop(&mut self) {
        let mut counts = self
            .admission
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.global {
            counts.total -= 1;
        }
        if let Some(ip) = self.ip {
            if let Some(from_ip) = counts.per_ip.get_mut(&ip) {
                *from_ip -= 1;
                if *from_ip == 0 {
                    counts.per_ip.remove(&ip);
                }
            }
        }
    }
}

/// Splits off a second handle to the socket. The login deadline shuts the socket
/// down through it: russh handles disconnect requests only after key exchange, so a
/// stalled exchange would otherwise keep its socket open.
pub(crate) fn duplicate_socket(
    stream: TcpStream,
) -> std::io::Result<(TcpStream, std::net::TcpStream)> {
    let stream = stream.into_std()?;
    let control = stream.try_clone()?;
    Ok((TcpStream::from_std(stream)?, control))
}

/// Serves one client from the accepted socket until its session ends. The client
/// must authenticate before `deadline`, or its socket is shut down.
async fn serve_connection<H: ConnectionHandler + 'static>(
    config: Arc<Config>,
    mut stream: TcpStream,
    peer: SocketAddr,
    trusted_proxy_ips: Arc<HashSet<IpAddr>>,
    mut handler: H,
    slot: PreAuthSlot,
    deadline: tokio::time::Instant,
) {
    let source = match connection_source(&mut stream, peer, &trusted_proxy_ips).await {
        Ok(source) => source,
        Err(error) => {
            warn!("Rejected SSH proxy header from {}: {}", peer, error);
            return;
        }
    };
    handler.set_client_ip(source.ip());
    if handler.client_is_blocked().await {
        warn!("Refused SSH connection from banned address {}", source.ip());
        return;
    }
    let Some(slot) = slot.for_ip(source.ip()) else {
        warn!(
            "Refused SSH connection from {}: too many unauthenticated connections",
            source.ip()
        );
        return;
    };
    let (stream, control) = match duplicate_socket(stream) {
        Ok(sockets) => sockets,
        Err(error) => {
            warn!(
                "Failed to prepare SSH socket from {}: {}",
                source.ip(),
                error
            );
            return;
        }
    };
    if config.nodelay {
        if let Err(error) = stream.set_nodelay(true) {
            warn!("Failed to set SSH TCP_NODELAY: {}", error);
        }
    }
    let (signal, authenticated) = oneshot::channel();
    handler.set_authenticated_signal(signal);
    let session =
        match tokio::time::timeout_at(deadline, russh::server::run_stream(config, stream, handler))
            .await
        {
            Ok(Ok(session)) => session,
            Ok(Err(error)) => {
                warn!("SSH connection setup failed: {}", error);
                return;
            }
            Err(_) => {
                warn!("SSH client {} did not identify itself in time", source.ip());
                return;
            }
        };
    // The slot is released when the client authenticates or when the session ends first (the
    // handler drops its signal then). The socket closes when this function returns, so
    // cancelling the connection also ends an authenticated session.
    let control = ShutdownOnDrop(control);
    let control_ref = &control;
    let login = async move {
        let outcome = tokio::time::timeout_at(deadline, authenticated).await;
        if !matches!(outcome, Ok(Ok(AuthOutcome::Accepted))) {
            let _ = control_ref.0.shutdown(std::net::Shutdown::Both);
        }
        // Authentication frees the global budget, not the source's connection budget.
        let mut slot = slot;
        slot.release_global();
        slot
    };
    let (_source_slot, result) = tokio::join!(login, session);
    if let Err(error) = result {
        warn!("SSH session failed: {}", error);
    }
}

fn server_config(private_key: PrivateKey) -> Config {
    Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
        window_size: 2 * 1024 * 1024,
        maximum_packet_size: 32 * 1024,
        channel_buffer_size: 2,
        auth_rejection_time: std::time::Duration::from_secs(3),
        auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
        keys: vec![private_key],
        // Only keys authenticate; clients should not offer a password.
        methods: russh::MethodSet::from(&[russh::MethodKind::PublicKey][..]),
        ..Default::default()
    }
}

/// Connection admission and shutdown are shared by native Git and router sessions.
pub trait ConnectionHandler:
    russh::server::Handler<Error = anyhow::Error> + russh::server::Server<Handler = Self> + Sized + Send
{
    fn set_client_ip(&mut self, ip: IpAddr);
    fn set_authenticated_signal(&mut self, signal: oneshot::Sender<AuthOutcome>);
    fn client_is_blocked(&self) -> impl std::future::Future<Output = bool> + Send;
    fn drain(&self) -> impl std::future::Future<Output = ()> + Send;
}
impl ConnectionHandler for GitSshHandler {
    fn set_client_ip(&mut self, ip: IpAddr) {
        GitSshHandler::set_client_ip(self, ip);
    }
    fn set_authenticated_signal(&mut self, signal: oneshot::Sender<AuthOutcome>) {
        GitSshHandler::set_authenticated_signal(self, signal);
    }
    async fn client_is_blocked(&self) -> bool {
        GitSshHandler::client_is_blocked(self).await
    }
    async fn drain(&self) {
        self.git_tasks.drain().await;
    }
}

pub struct SshServer<H = GitSshHandler> {
    config: Arc<Config>,
    handler: H,
    trusted_proxy_ips: Arc<HashSet<IpAddr>>,
    admission: PreAuthAdmission,
}

impl SshServer {
    pub fn with_router_authorization(
        mut self,
        authorization: Arc<dyn crate::handler::RouterAuthorization>,
    ) -> Self {
        self.handler.router_authorization = Some(authorization);
        self
    }

    pub async fn new(
        pool: DbPool,
        host_key_path: Option<&str>,
        ci_service: Arc<CiRuns>,
    ) -> Result<Self> {
        let cluster_key_pem = if let Some(key_pem) =
            codeyard_db_placement::node_config::get_cluster_ssh_host_key(&pool).await?
        {
            info!("Loaded cluster SSH host key from database");
            key_pem
        } else {
            let bootstrap_key = if let Some(path) = host_key_path {
                if Path::new(path)
                    .try_exists()
                    .with_context(|| format!("Failed to inspect host key at {}", path))?
                {
                    use std::io::Read;
                    let mut key = String::new();
                    open_host_key_file(path)?
                        .take(1024 * 1024)
                        .read_to_string(&mut key)?;
                    info!("Bootstrapping cluster SSH host key from {}", path);
                    key
                } else {
                    new_host_key()
                        .to_openssh(russh::keys::ssh_key::LineEnding::LF)?
                        .to_string()
                }
            } else {
                new_host_key()
                    .to_openssh(russh::keys::ssh_key::LineEnding::LF)?
                    .to_string()
            };

            // Only a usable, unencrypted key may win the first-writer race. A bad
            // configured file must not permanently poison the shared database.
            let key = russh::keys::decode_secret_key(&bootstrap_key, None)
                .context("Failed to decode configured SSH host key")?;
            anyhow::ensure!(
                Config::default()
                    .preferred
                    .key
                    .iter()
                    .any(|algorithm| *algorithm == key.algorithm()),
                "Unsupported SSH host key algorithm"
            );
            codeyard_db_placement::node_config::insert_cluster_ssh_host_key(&pool, &bootstrap_key)
                .await?;
            let stored_key = codeyard_db_placement::node_config::get_cluster_ssh_host_key(&pool)
                .await?
                .context("Cluster SSH host key missing after initialization")?;
            info!("Loaded initialized cluster SSH host key from database");
            stored_key
        };

        let private_key = russh::keys::decode_secret_key(&cluster_key_pem, None)
            .context("Failed to decode cluster SSH host key from database")?;

        if let Some(path) = host_key_path {
            use std::io::Read;
            let current = match open_host_key_file(path) {
                Ok(file) => {
                    let mut bytes = String::new();
                    file.take(1024 * 1024).read_to_string(&mut bytes)?;
                    Some(bytes)
                }
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            if current.as_deref() != Some(cluster_key_pem.as_str()) {
                write_host_key_file(path, &cluster_key_pem)?;
                info!("Synchronized SSH host key file at {}", path);
            }
            // Matching bytes do not imply safe metadata; inspect the opened descriptor too.
            open_host_key_file(path)?;
        }

        let config = server_config(private_key);

        let rate_limiter = RateLimiter::new();
        let handler = GitSshHandler::new(pool, rate_limiter, ci_service);
        let trusted_proxy_ips = Arc::new(configured_trusted_proxy_ips()?);

        Ok(Self {
            config: Arc::new(config),
            handler,
            trusted_proxy_ips,
            admission: PreAuthAdmission::default(),
        })
    }
}

impl<H: ConnectionHandler + 'static> SshServer<H> {
    pub fn with_handler(host_key_path: &str, handler: H) -> Result<Self> {
        use std::io::Read;
        let key = match open_host_key_file(host_key_path) {
            Ok(file) => {
                let mut pem = String::new();
                file.take(1024 * 1024).read_to_string(&mut pem)?;
                russh::keys::decode_secret_key(&pem, None)?
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                let key = new_host_key();
                write_host_key_file(
                    host_key_path,
                    &key.to_openssh(russh::keys::ssh_key::LineEnding::LF)?,
                )?;
                key
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            config: Arc::new(server_config(key)),
            handler,
            trusted_proxy_ips: Arc::new(configured_trusted_proxy_ips()?),
            admission: PreAuthAdmission::default(),
        })
    }

    /// Accepts connections until `stop` is set. Then it closes the listener and returns
    /// once connections and Git tasks have ended, so the caller bounds that wait.
    pub async fn run(&mut self, addr: SocketAddr, stop: watch::Receiver<bool>) -> Result<()> {
        self.run_listener(TcpListener::bind(addr).await?, stop)
            .await
    }

    pub async fn run_listener(
        &mut self,
        socket: TcpListener,
        mut stop: watch::Receiver<bool>,
    ) -> Result<()> {
        info!("Starting SSH server on {}", socket.local_addr()?);
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
                accepted = socket.accept() => match accepted {
                    Ok((stream, peer)) => {
                        // An authenticated session releases its admission slot but stays in
                        // this set until it ends.
                        if connections.len() >= MAX_CONNECTIONS {
                            continue;
                        }
                        // Delayed PROXY headers must consume the global cap too.
                        let Some(slot) = self.admission.reserve() else { continue; };
                        connections.spawn(serve_connection(
                            self.config.clone(),
                            stream,
                            peer,
                            self.trusted_proxy_ips.clone(),
                            self.handler.new_client(Some(peer)),
                            slot,
                            tokio::time::Instant::now() + LOGIN_GRACE_TIME,
                        ));
                    }
                    // Accept fails when descriptors run out. The pending connection stays
                    // queued, so pause and retry instead of ending the listener.
                    Err(error) => {
                        warn!("SSH accept failed: {}", error);
                        tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    }
                },
            }
        }
        drop(socket);
        info!("SSH server stopped accepting connections");
        tokio::join!(
            async { while connections.join_next().await.is_some() {} },
            self.handler.drain(),
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod request_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use russh::server::Server as _;
    use tokio::io::AsyncWriteExt;

    async fn tcp_pair() -> (TcpStream, TcpStream, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        let client = client.unwrap();
        let (server, peer) = accepted.unwrap();
        (client, server, peer)
    }

    async fn forwarded_source(header: &str) -> SocketAddr {
        let (mut router, mut server, peer) = tcp_pair().await;
        router.write_all(header.as_bytes()).await.unwrap();
        let trusted = HashSet::from([peer.ip()]);
        connection_source(&mut server, peer, &trusted)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn trusted_proxy_sources_use_separate_failure_buckets() {
        let first =
            forwarded_source("PROXY TCP4 192.0.2.10 198.51.100.10 41000 22\r\nSSH-2.0-test\r\n")
                .await;
        let second =
            forwarded_source("PROXY TCP4 192.0.2.11 198.51.100.10 41001 22\r\nSSH-2.0-test\r\n")
                .await;
        assert_eq!(first, "192.0.2.10:41000".parse().unwrap());
        assert_eq!(second, "192.0.2.11:41001".parse().unwrap());

        let limiter = RateLimiter::new();
        for attempt in 0..5 {
            assert_eq!(limiter.record_failure(first.ip()).await, attempt == 4);
        }
        assert!(limiter.is_blocked(first.ip()).await);
        assert!(!limiter.is_blocked(second.ip()).await);
    }

    #[tokio::test]
    async fn direct_ssh_keeps_tcp_source_and_cannot_supply_proxy_metadata() {
        let (mut direct, mut server, peer) = tcp_pair().await;
        direct.write_all(b"SSH-2.0-direct\r\n").await.unwrap();
        let trusted = HashSet::from([peer.ip()]);
        assert_eq!(
            connection_source(&mut server, peer, &trusted)
                .await
                .unwrap(),
            peer
        );
        let mut prefix = [0; 6];
        server.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"SSH-2.");

        let (mut attacker, mut server, peer) = tcp_pair().await;
        attacker
            .write_all(b"PROXY TCP4 203.0.113.9 198.51.100.10 1234 22\r\nSSH-2.0-test\r\n")
            .await
            .unwrap();
        assert_eq!(
            connection_source(&mut server, peer, &HashSet::new())
                .await
                .unwrap(),
            peer
        );
        server.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"PROXY ");
    }

    async fn test_parts() -> (DbPool, Arc<CiRuns>) {
        let native = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .map(codeyard_database::pool::Pool::from)
            .unwrap();
        let pool = DbPool::from(native);
        codeyard_database::run_migrations(&pool).await.unwrap();
        let ci = Arc::new(CiRuns::new(
            pool.clone(),
            Arc::new(codeyard_ci::config::CiConfig::default()),
            Arc::new(tokio::sync::Notify::new()),
            None,
            Arc::new(codeyard_notifications::broadcast::NotificationBroadcaster::new(&pool)),
        ));
        (pool, ci)
    }

    async fn test_server() -> SshServer {
        let (pool, ci) = test_parts().await;
        SshServer::new(pool, None, ci).await.unwrap()
    }

    /// The server writes its SSH identification as soon as it admits a connection.
    async fn expect_admitted(client: &mut TcpStream) {
        let mut prefix = [0; 4];
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_exact(&mut prefix),
        )
        .await
        .expect("connection was not admitted")
        .unwrap();
        assert_eq!(&prefix, b"SSH-");
    }

    async fn connect_when_listening(address: SocketAddr) -> TcpStream {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(stream) = TcpStream::connect(address).await {
                    return stream;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("SSH listener did not start")
    }

    #[test]
    fn preauth_slots_cap_each_address_and_the_total_until_released() {
        let admission = PreAuthAdmission::default();
        let admit = |ip| admission.reserve()?.for_ip(ip);
        let mut slots = Vec::new();
        for host in 1..=(MAX_PREAUTH_CONNECTIONS / MAX_PREAUTH_CONNECTIONS_PER_IP) {
            let ip = IpAddr::from([192, 0, 2, host as u8]);
            for _ in 0..MAX_PREAUTH_CONNECTIONS_PER_IP {
                slots.push(admit(ip).unwrap());
            }
            assert!(admit(ip).is_none());
        }
        let other = IpAddr::from([192, 0, 2, 200]);
        assert!(admit(other).is_none());
        slots.pop();
        assert!(admit(other).is_some());
    }

    #[test]
    fn preauth_slots_count_an_ipv6_prefix_as_one_address() {
        let admission = PreAuthAdmission::default();
        let admit = |ip: &str| admission.reserve()?.for_ip(ip.parse().unwrap());
        let slots: Vec<_> = (1..=MAX_PREAUTH_CONNECTIONS_PER_IP)
            .map(|host| admit(&format!("2001:db8:0:1::{host:x}")).unwrap())
            .collect();
        assert!(admit("2001:db8:0:1::ffff").is_none());
        assert!(admit("2001:db8:0:2::1").is_some());
        drop(slots);
    }

    #[tokio::test]
    async fn incomplete_proxy_headers_consume_the_global_preauth_cap() {
        let mut server = test_server().await;
        server.trusted_proxy_ips = Arc::new(HashSet::from(["127.0.0.1".parse().unwrap()]));
        let admission = server.admission.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = watch::channel(false);
        let running = tokio::spawn(async move { server.run_listener(listener, stopped).await });
        let slots: Vec<_> = (0..MAX_PREAUTH_CONNECTIONS - 1)
            .map(|_| admission.reserve().unwrap())
            .collect();
        let mut client = connect_when_listening(address).await;
        client.write_all(b"PROXY ").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(800), async {
            while admission.0.lock().unwrap().total != MAX_PREAUTH_CONNECTIONS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut refused = TcpStream::connect(address).await.unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_millis(800),
                refused.read(&mut byte)
            )
            .await
            .unwrap()
            .unwrap(),
            0
        );
        drop(client);
        drop(slots);
        stop.send_replace(true);
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn unauthenticated_connections_are_shut_down_at_the_login_deadline() {
        let mut server = test_server().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        let mut client = client.unwrap();
        let (stream, peer) = accepted.unwrap();
        let served = tokio::spawn(serve_connection(
            server.config.clone(),
            stream,
            peer,
            server.trusted_proxy_ips.clone(),
            server.handler.new_client(Some(peer)),
            PreAuthAdmission::default().reserve().unwrap(),
            tokio::time::Instant::now() + std::time::Duration::from_millis(300),
        ));
        // Identify as an SSH client, then never authenticate.
        client.write_all(b"SSH-2.0-test\r\n").await.unwrap();
        let mut rest = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.read_to_end(&mut rest),
        )
        .await
        .expect("the login deadline did not close the socket")
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), served)
            .await
            .expect("the connection task kept running")
            .unwrap();
    }

    #[tokio::test]
    async fn banned_addresses_are_refused_before_the_ssh_handshake() {
        let server = test_server().await;
        let (pool, ci) = test_parts().await;
        let limiter = RateLimiter::new();
        for _ in 0..5 {
            limiter.record_failure("127.0.0.1".parse().unwrap()).await;
        }
        let mut handler = GitSshHandler::new(pool, limiter, ci);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        let mut client = client.unwrap();
        let (stream, peer) = accepted.unwrap();
        let served = tokio::spawn(serve_connection(
            server.config.clone(),
            stream,
            peer,
            server.trusted_proxy_ips.clone(),
            handler.new_client(Some(peer)),
            PreAuthAdmission::default().reserve().unwrap(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        ));
        let mut byte = [0; 1];
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), client.read(&mut byte))
            .await
            .expect("banned connection was left open")
            .unwrap();
        assert_eq!(read, 0);
        served.await.unwrap();
    }

    #[tokio::test]
    async fn only_public_keys_are_offered() {
        let server = test_server().await;
        assert!(server
            .config
            .methods
            .contains(&russh::MethodKind::PublicKey));
        assert!(!server.config.methods.contains(&russh::MethodKind::Password));
        assert!(!server
            .config
            .methods
            .contains(&russh::MethodKind::KeyboardInteractive));
    }

    #[tokio::test]
    async fn per_address_limit_refuses_extra_connections_and_stop_waits_for_open_ones() {
        let mut server = test_server().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = watch::channel(false);
        let mut running = tokio::spawn(async move { server.run_listener(listener, stopped).await });

        let mut admitted = Vec::new();
        for _ in 0..MAX_PREAUTH_CONNECTIONS_PER_IP {
            let mut client = connect_when_listening(address).await;
            expect_admitted(&mut client).await;
            admitted.push(client);
        }
        // One more connection from the same address is closed before identification.
        let mut refused = TcpStream::connect(address).await.unwrap();
        let mut byte = [0; 1];
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), refused.read(&mut byte))
            .await
            .expect("refused connection was left open")
            .unwrap();
        assert_eq!(read, 0);

        stop.send_replace(true);
        // The listener closes first, so new connections are refused.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while TcpStream::connect(address).await.is_ok() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("listener stayed open after stop");
        // run() returns only after the open connections have ended.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut running)
                .await
                .is_err()
        );
        drop(admitted);
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), running)
            .await
            .expect("open connections kept run() alive")
            .unwrap();
        assert!(result.is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn host_key_files_are_private_when_created_and_rewritten() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("keys/ssh_host_ed25519_key");
        let path_text = path.to_str().unwrap();
        write_host_key_file(path_text, "first key with longer contents").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "first key with longer contents"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_host_key_file(path_text, "second key").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second key");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn host_key_file_errors_are_returned() {
        let directory = tempfile::tempdir().unwrap();
        assert!(write_host_key_file(directory.path().to_str().unwrap(), "key").is_err());

        let parent = directory.path().join("not-a-directory");
        std::fs::write(&parent, "existing contents").unwrap();
        let path = parent.join("ssh_host_ed25519_key");
        assert!(write_host_key_file(path.to_str().unwrap(), "key").is_err());
        assert_eq!(
            std::fs::read_to_string(parent).unwrap(),
            "existing contents"
        );
    }
}
