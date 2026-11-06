//! One request stream between two nodes, whichever transport carried it. Protocol code
//! reads and writes a `Link`; the transport modules open and accept them.
use iroh::endpoint::Connection;
use iroh::PublicKey;
use std::fmt;
use std::io;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

/// One request stream and the authenticated key of the node at its other end.
pub struct Link {
    /// The node key the transport verified. Membership checks use it.
    pub peer: PublicKey,
    /// The protocol name both sides agreed on.
    pub alpn: Vec<u8>,
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    carrier: Carrier,
}

/// What carries the stream. A QUIC stream belongs to a connection that must be closed
/// with it. A TCP connection carries one stream, so dropping the halves closes it.
enum Carrier {
    Quic(Box<Connection>),
    Tcp,
}

impl fmt::Debug for Link {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Link")
            .field("peer", &self.peer)
            .field("alpn", &String::from_utf8_lossy(&self.alpn))
            .finish_non_exhaustive()
    }
}

impl Link {
    pub fn quic(
        peer: PublicKey,
        alpn: &[u8],
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
        connection: Connection,
    ) -> Link {
        Link {
            peer,
            alpn: alpn.to_vec(),
            reader: Box::new(reader),
            writer: Box::new(writer),
            carrier: Carrier::Quic(Box::new(connection)),
        }
    }

    /// A TLS connection over TCP, already split into its halves.
    pub fn tcp(
        peer: PublicKey,
        alpn: &[u8],
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Link {
        Link {
            peer,
            alpn: alpn.to_vec(),
            reader: Box::new(reader),
            writer: Box::new(writer),
            carrier: Carrier::Tcp,
        }
    }

    /// Ends our side of the stream, so the peer reads end of stream.
    pub async fn finish(&mut self) -> io::Result<()> {
        self.writer.shutdown().await
    }

    /// Ends the connection with a code and a reason the peer can read. TCP has no codes or
    /// reasons: closing a TCP link just drops it.
    pub fn close(self, code: u32, reason: &[u8]) {
        if let Carrier::Quic(connection) = self.carrier {
            connection.close(code.into(), reason);
        }
    }

    /// Waits until the peer ends the connection. Over TCP that is the end of the peer's
    /// stream, which the peer sends when it finishes or drops the connection.
    pub async fn wait_for_peer_close(&mut self) {
        match &self.carrier {
            Carrier::Quic(connection) => {
                let connection = connection.clone();
                let _ = connection.closed().await;
            }
            Carrier::Tcp => {
                let _ = tokio::io::copy(&mut self.reader, &mut tokio::io::sink()).await;
            }
        }
    }
}

/// Reads to the end of the stream. A message longer than `limit` is an error.
pub async fn read_to_end_within(
    reader: &mut (impl AsyncRead + Unpin),
    limit: usize,
) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    // One byte past the limit tells a message that is too long from one that fits.
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message exceeds its limit",
        ));
    }
    Ok(bytes)
}

/// Check the length before allocating, including zero-length binary bodies.
pub async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin + ?Sized),
    limit: usize,
) -> anyhow::Result<Vec<u8>> {
    let length = reader.read_u32().await? as usize;
    anyhow::ensure!(length <= limit, "RPC frame exceeds its limit");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

pub async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin + ?Sized),
    bytes: &[u8],
    limit: usize,
) -> anyhow::Result<()> {
    anyhow::ensure!(bytes.len() <= limit, "RPC frame exceeds its limit");
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    // A TLS writer can accept the bytes while records still wait behind a full socket. The
    // peer may need this frame before it sends anything, so push it out now.
    writer.flush().await?;
    Ok(())
}
