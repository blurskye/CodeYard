use crate::link::Link;
use anyhow::{bail, Context, Result};
use codeyard_support::node_identity::NodeKey;
use iroh::PublicKey;
use std::{net::SocketAddr, time::Duration};
#[cfg(test)]
use tokio::net::TcpListener;
use tokio::{io::split, net::TcpStream};
use tokio_rustls::{rustls::pki_types::ServerName, TlsConnector};
use tracing::warn;
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
    let config = crate::tls::client_config(key, node, alpn)?;
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
pub fn tune(socket: &TcpStream) {
    if let Err(error) = socket.set_nodelay(true) {
        warn!(%error, "cannot turn off Nagle's algorithm on a cluster connection");
    }
    #[cfg(target_os = "linux")]
    if let Err(error) =
        socket2::SockRef::from(socket).set_tcp_user_timeout(Some(Duration::from_secs(15)))
    {
        warn!(%error, "cannot bound unacknowledged data on a cluster connection");
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
