//! Node connections over TCP. The listener takes TLS connections (see `tls`), checks the
//! peer's key against membership, and passes the one request on the connection to the
//! same protocol code that the iroh router serves.
use super::{
    ci::CiHandler,
    identity::NodeKey,
    link::Link,
    pairing::{self, PairingHandler},
    rpc::RpcHandler,
    tls,
    transport::{active_member, ALPN_PAIR, ALPN_RPC},
};
use crate::backend::db::DbPool;
use anyhow::{bail, Context, Result};
use iroh::PublicKey;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::split,
    net::{TcpListener, TcpStream},
    sync::{watch, OwnedSemaphorePermit, Semaphore},
    task::{JoinHandle, JoinSet},
    time::Instant,
};
use tokio_rustls::{rustls::pki_types::ServerName, TlsAcceptor, TlsConnector};
use tracing::{debug, warn};

/// How long a peer has, from the moment it connects, to finish the TLS handshake and the
/// membership check. One deadline covers both.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Handshakes in progress at once. A peer that arrives over the limit is closed at once.
const MAX_HANDSHAKES: usize = 64;

/// What a node serves over TCP, using the same handlers as the iroh router: pairing
/// always, repository calls only on storage nodes, and CI queue requests.
#[derive(Clone)]
pub struct Services {
    pub pool: DbPool,
    pub pairing: PairingHandler,
    pub rpc: Option<RpcHandler>,
    pub events: CiHandler,
}

/// The accept loop and the connections it started. `stop` ends both and waits for them to
/// finish. Dropping the listener aborts them without waiting.
pub struct Listener {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    /// Closes when the accept loop has ended. The loop holds its only sender.
    ended: watch::Receiver<()>,
    task: JoinHandle<()>,
}

impl Listener {
    /// The address the listener is bound to, with the port filled in.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting and ends the connections the listener started. The listening socket
    /// is closed when this returns, so the address can be bound again at once.
    pub async fn stop(&self) {
        self.stop.send_replace(true);
        // The wait ends when the loop drops its sender, which it does only after it has
        // closed the listening socket and every connection.
        let _ = self.ended.clone().changed().await;
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Listens for node connections on `address`.
pub async fn bind(address: SocketAddr, key: &NodeKey, services: Services) -> Result<Listener> {
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("cannot listen for cluster connections on {address}"))?;
    let addr = listener.local_addr()?;
    let acceptor = TlsAcceptor::from(tls::server_config(key)?);
    let handshakes = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    let (stop, stop_signal) = watch::channel(false);
    let (ended_sender, ended) = watch::channel(());
    let task = tokio::spawn(accept_loop(
        listener,
        acceptor,
        handshakes,
        services,
        stop_signal,
        ended_sender,
    ));
    Ok(Listener {
        addr,
        stop,
        ended,
        task,
    })
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    handshakes: Arc<Semaphore>,
    services: Services,
    mut stop: watch::Receiver<bool>,
    ended: watch::Sender<()>,
) {
    // Owned here, so the connections end with the loop, however the loop ends.
    let mut connections = JoinSet::new();
    loop {
        let socket = tokio::select! {
            _ = stop.changed() => break,
            accepted = listener.accept() => match accepted {
                Ok((socket, _)) => socket,
                Err(error) => {
                    // Usually out of file descriptors: wait a moment rather than spin.
                    warn!(%error, "cannot accept a cluster connection");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        // Finished connections leave the set here, so it holds only live tasks.
        while connections.try_join_next().is_some() {}
        let Ok(permit) = handshakes.clone().try_acquire_owned() else {
            debug!("too many cluster handshakes in progress; closing a new connection");
            continue;
        };
        tune(&socket);
        connections.spawn(serve_socket(
            socket,
            permit,
            acceptor.clone(),
            services.clone(),
        ));
    }
    // Close the listening socket, then wait for every connection to end. The loop ends, and
    // `Listener::stop` returns, only after both.
    drop(listener);
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    drop(ended);
}

/// One connection: the handshake, the membership check, then the one request.
async fn serve_socket(
    socket: TcpStream,
    permit: OwnedSemaphorePermit,
    acceptor: TlsAcceptor,
    services: Services,
) {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let handshake = tokio::time::timeout_at(deadline, acceptor.accept(socket)).await;
    let stream = match handshake {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            debug!(%error, "cluster TLS handshake failed");
            return;
        }
        Err(_) => {
            debug!("cluster TLS handshake timed out");
            return;
        }
    };
    let Some(alpn) = stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec) else {
        debug!("cluster connection named no protocol");
        return;
    };
    let peer = match stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first())
    {
        Some(certificate) => match tls::peer_key(certificate) {
            Ok(peer) => peer,
            Err(error) => {
                debug!(%error, "cluster peer key rejected");
                return;
            }
        },
        None => {
            debug!("cluster peer presented no key");
            return;
        }
    };
    // The slot covers the membership lookup too. A lookup that stalls holds the slot until
    // it ends or times out, so unpaired peers cannot pile up past the limit.
    let allowed = tokio::time::timeout_at(deadline, admitted(&services, alpn.as_slice(), peer))
        .await
        .unwrap_or_else(|_| {
            debug!("cluster membership check timed out");
            false
        });
    drop(permit);
    if !allowed {
        return;
    }
    let (reader, writer) = split(stream);
    let link = Link::tcp(peer, &alpn, reader, writer);
    let outcome = if alpn.as_slice() == ALPN_PAIR {
        // The slot is taken before anything is read, as over iroh. A connection that finds
        // every slot in use is closed at once.
        match services.pairing.slot() {
            Some(slot) => {
                services
                    .pairing
                    .serve_link(link, Instant::now() + pairing::EXCHANGE_TIMEOUT, slot)
                    .await
            }
            None => debug!("too many pairing requests; closing a connection"),
        }
        Ok(())
    } else if alpn.as_slice() == ALPN_RPC {
        match &services.rpc {
            Some(rpc) => rpc.serve_link(link).await,
            None => Ok(()),
        }
    } else {
        services.events.serve_link(link).await
    };
    if let Err(error) = outcome {
        debug!(%error, "cluster request failed");
    }
}

/// The check the iroh hook makes after its handshake, with the same answers: pairing is
/// open to any key, and every other protocol needs an active member.
async fn admitted(services: &Services, alpn: &[u8], peer: PublicKey) -> bool {
    if alpn == ALPN_PAIR {
        return true;
    }
    if alpn == ALPN_RPC && services.rpc.is_none() {
        // A node without repository storage does not serve this protocol.
        return false;
    }
    match active_member(&services.pool, peer).await {
        Ok(member) => member,
        Err(error) => {
            // Without an answer the safe choice is to refuse.
            warn!(%error, "could not check cluster membership");
            false
        }
    }
}

/// Opens a request stream to `node` at `address`. The handshake must end with the key of
/// `node`, and the node must agree to `alpn`. The caller sets the time limit.
pub async fn dial(
    key: &NodeKey,
    address: SocketAddr,
    node: PublicKey,
    alpn: &[u8],
) -> Result<Link> {
    let socket = TcpStream::connect(address)
        .await
        .with_context(|| format!("cannot connect to {address}"))?;
    tune(&socket);
    let config = tls::client_config(key, node, alpn)?;
    let stream = TlsConnector::from(config)
        .connect(ServerName::IpAddress(address.ip().into()), socket)
        .await
        .with_context(|| format!("the TLS handshake with {address} failed"))?;
    if stream.get_ref().1.alpn_protocol() != Some(alpn) {
        bail!("the node at {address} did not agree to the protocol");
    }
    let (reader, writer) = split(stream);
    Ok(Link::tcp(node, alpn, reader, writer))
}

/// Settings every node connection gets. TCP keepalive makes a peer that vanished while the
/// connection was idle show up in about fifteen seconds: five seconds of silence, then two
/// probes five seconds apart.
fn tune(socket: &TcpStream) {
    if let Err(error) = socket.set_nodelay(true) {
        warn!(%error, "cannot turn off Nagle's algorithm on a cluster connection");
    }
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(5))
        .with_interval(Duration::from_secs(5))
        .with_retries(2);
    if let Err(error) = socket2::SockRef::from(socket).set_tcp_keepalive(&keepalive) {
        warn!(%error, "cannot set TCP keepalive on a cluster connection");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn both_ends_of_a_node_connection_ask_for_keepalive() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (dialed, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        let dialed = dialed.unwrap();
        let (accepted, _) = accepted.unwrap();
        tune(&dialed);
        tune(&accepted);
        for socket in [&dialed, &accepted] {
            assert!(socket2::SockRef::from(socket).keepalive().unwrap());
        }
    }
}
