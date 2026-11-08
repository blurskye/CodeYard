//! Bounded node-key TLS request/response transport shared by directory and cell RPC.
use anyhow::{ensure, Result};
use codeyard_nodes::{link::read_to_end_within, tcp, tls};
use codeyard_support::node_identity::NodeKey;
use futures_util::future::BoxFuture;
use iroh::PublicKey;
use serde::{de::DeserializeOwned, Serialize};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{split, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinSet,
};
use tokio_rustls::TlsAcceptor;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

#[derive(Clone, Copy)]
pub struct Config {
    pub exchange_seconds: u64,
    pub maximum_bytes: usize,
    pub maximum_connections: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            exchange_seconds: 10,
            maximum_bytes: 4 * 1024 * 1024,
            maximum_connections: 128,
        }
    }
}
impl Config {
    pub fn validate(self) -> Result<()> {
        ensure!(
            (1..=60).contains(&self.exchange_seconds)
                && (1024..=16 * 1024 * 1024).contains(&self.maximum_bytes)
                && (1..=4096).contains(&self.maximum_connections),
            "Invalid node RPC limits"
        );
        Ok(())
    }
}
#[derive(Clone)]
pub struct Client {
    key: NodeKey,
    pub address: SocketAddr,
    pub peer: PublicKey,
    alpn: &'static [u8],
    config: Config,
}
impl Client {
    pub fn new(
        key: NodeKey,
        address: SocketAddr,
        peer: PublicKey,
        alpn: &'static [u8],
        config: Config,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            key,
            address,
            peer,
            alpn,
            config,
        })
    }
    pub fn config(&self) -> Config {
        self.config
    }
    /// A streamed reply uses the existing Git binary frames and a final bounded JSON frame.
    /// The caller owns the file and discards it unless the final reply authorizes publication.
    pub async fn response_stream<T: Serialize + Sync, R: DeserializeOwned>(
        &self,
        request: &T,
        output: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
        maximum_bytes: u64,
        seconds: u64,
    ) -> Result<R> {
        use codeyard_nodes::link::read_frame;
        ensure!(
            (1..=64 * 1024 * 1024 * 1024).contains(&maximum_bytes) && (1..=3600).contains(&seconds),
            "Invalid snapshot transfer limits"
        );
        let bytes = serde_json::to_vec(request)?;
        ensure!(
            bytes.len() <= self.config.maximum_bytes,
            "Node RPC request is too large"
        );
        tokio::time::timeout(Duration::from_secs(seconds), async {
            let mut link = tcp::dial(&self.key, self.address, self.peer, self.alpn).await?;
            link.writer.write_all(&bytes).await?;
            link.finish().await?;
            let mut received = 0_u64;
            loop {
                let frame = tokio::time::timeout(
                    Duration::from_secs(30),
                    read_frame(&mut link.reader, 32768),
                )
                .await??;
                if frame.is_empty() {
                    break;
                }
                received = received
                    .checked_add(frame.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Snapshot exceeds its limit"))?;
                ensure!(received <= maximum_bytes, "Snapshot exceeds its limit");
                output.write_all(&frame).await?;
            }
            output.flush().await?;
            let reply = tokio::time::timeout(
                Duration::from_secs(30),
                read_frame(&mut link.reader, self.config.maximum_bytes),
            )
            .await??;
            Ok(serde_json::from_slice(&reply)?)
        })
        .await?
    }
    pub async fn request<T: Serialize + Sync, R: DeserializeOwned>(
        &self,
        request: &T,
    ) -> Result<R> {
        let bytes = serde_json::to_vec(request)?;
        ensure!(
            bytes.len() <= self.config.maximum_bytes,
            "Node RPC request is too large"
        );
        tokio::time::timeout(Duration::from_secs(self.config.exchange_seconds), async {
            let mut link = tcp::dial(&self.key, self.address, self.peer, self.alpn).await?;
            link.writer.write_all(&bytes).await?;
            link.finish().await?;
            let bytes = read_to_end_within(&mut link.reader, self.config.maximum_bytes).await?;
            Ok(serde_json::from_slice(&bytes)?)
        })
        .await?
    }
}
pub trait Service: Send + Sync {
    fn streams_reply(&self, _bytes: &[u8]) -> bool {
        false
    }
    fn stream_reply<'a>(
        &'a self,
        _peer: PublicKey,
        _bytes: &'a [u8],
        _output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { anyhow::bail!("Streaming RPC is unavailable") })
    }
    /// Enrollment is checked before reading the body. Operations recheck at their effect.
    fn admit<'a>(&'a self, peer: PublicKey) -> BoxFuture<'a, Result<()>>;
    fn handle<'a>(&'a self, peer: PublicKey, bytes: &'a [u8]) -> BoxFuture<'a, Result<Vec<u8>>>;
}
pub struct Listener {
    pub address: SocketAddr,
    cancel: CancellationToken,
    task: Option<AbortOnDropHandle<()>>,
}
impl Listener {
    pub async fn bind(
        service: Arc<dyn Service>,
        key: &NodeKey,
        address: SocketAddr,
        alpn: &'static [u8],
        config: Config,
    ) -> Result<Self> {
        config.validate()?;
        let acceptor = TlsAcceptor::from(tls::server_config_for(key, &[alpn])?);
        let listener = TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let task = tokio::spawn(async move {
            let slots = Arc::new(Semaphore::new(config.maximum_connections));
            let mut tasks = JoinSet::new();
            loop {
                while tasks.try_join_next().is_some() {}
                let accepted =
                    tokio::select! {_=stop.cancelled()=>break,accepted=listener.accept()=>accepted};
                let (socket, _) = match accepted {
                    Ok(socket) => socket,
                    Err(error) => {
                        tracing::warn!(%error,"node RPC listener failed");
                        break;
                    }
                };
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    continue;
                };
                let service = service.clone();
                let acceptor = acceptor.clone();
                tasks.spawn(async move {
                    let _slot = slot;
                    serve(socket, acceptor, service, alpn, config).await
                });
            }
            drop(listener);
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        });
        Ok(Self {
            address,
            cancel,
            task: Some(AbortOnDropHandle::new(task)),
        })
    }
    pub async fn ended(&mut self) {
        if let Some(task) = &mut self.task {
            let _ = task.await;
        }
        let _ = self.task.take();
    }
    pub async fn stop(mut self) {
        self.cancel.cancel();
        if let Some(mut task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(10), &mut task).await;
        }
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
async fn serve(
    socket: TcpStream,
    acceptor: TlsAcceptor,
    service: Arc<dyn Service>,
    alpn: &'static [u8],
    config: Config,
) {
    tcp::tune(&socket);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(config.exchange_seconds);
    let exchange = tokio::time::timeout_at(deadline, async {
        let stream = acceptor.accept(socket).await?;
        ensure!(
            stream.get_ref().1.alpn_protocol() == Some(alpn),
            "Invalid node RPC protocol"
        );
        let cert = stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|certs| certs.first())
            .ok_or_else(|| anyhow::anyhow!("Missing node identity"))?;
        let peer = tls::peer_key(cert)?;
        service.admit(peer).await?;
        let (mut reader, writer) = split(stream);
        let bytes = read_to_end_within(&mut reader, config.maximum_bytes).await?;
        anyhow::Ok((peer, writer, bytes))
    })
    .await;
    let Ok(Ok((peer, mut writer, bytes))) = exchange else {
        return;
    };
    if service.streams_reply(&bytes) {
        let _ = tokio::time::timeout(Duration::from_secs(300), async {
            service.stream_reply(peer, &bytes, &mut writer).await?;
            writer.shutdown().await?;
            anyhow::Ok(())
        })
        .await;
    } else {
        let _ = tokio::time::timeout_at(deadline, async {
            let bytes = service.handle(peer, &bytes).await?;
            ensure!(
                bytes.len() <= config.maximum_bytes,
                "Node RPC response is too large"
            );
            writer.write_all(&bytes).await?;
            writer.shutdown().await?;
            anyhow::Ok(())
        })
        .await;
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use codeyard_nodes::link::write_frame;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Source {
        peer: PublicKey,
        active: AtomicBool,
        revoke: bool,
    }
    impl Service for Source {
        fn admit<'a>(&'a self, peer: PublicKey) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                ensure!(
                    peer == self.peer && self.active.load(Ordering::SeqCst),
                    "Source is revoked"
                );
                Ok(())
            })
        }
        fn handle<'a>(
            &'a self,
            peer: PublicKey,
            bytes: &'a [u8],
        ) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async move {
                self.admit(peer).await?;
                Ok(bytes.to_vec())
            })
        }
        fn streams_reply(&self, bytes: &[u8]) -> bool {
            bytes == b"null"
        }
        fn stream_reply<'a>(
            &'a self,
            peer: PublicKey,
            _bytes: &'a [u8],
            output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.admit(peer).await?;
                for _ in 0..2 {
                    write_frame(output, &vec![7; 32768], 32768).await?;
                }
                if self.revoke {
                    self.active.store(false, Ordering::SeqCst);
                }
                self.admit(peer).await?;
                write_frame(output, b"", 32768).await?;
                write_frame(output, br#"{"accepted":true}"#, 4096).await?;
                Ok(())
            })
        }
    }
    async fn fixture(revoke: bool) -> (Listener, Client) {
        let server = NodeKey::generate();
        let peer = NodeKey::generate();
        let listener = Listener::bind(
            Arc::new(Source {
                peer: peer.public_key(),
                active: AtomicBool::new(true),
                revoke,
            }),
            &server,
            "127.0.0.1:0".parse().unwrap(),
            b"codeyard/cells/stream-test",
            Config::default(),
        )
        .await
        .unwrap();
        let client = Client::new(
            peer,
            listener.address,
            server.public_key(),
            b"codeyard/cells/stream-test",
            Config::default(),
        )
        .unwrap();
        (listener, client)
    }
    #[tokio::test]
    async fn streams_use_binary_frames_and_keep_normal_rpc_replies() {
        let (listener, client) = fixture(false).await;
        let request = serde_json::json!({"metadata":"bounded"});
        assert_eq!(
            client
                .request::<_, serde_json::Value>(&request)
                .await
                .unwrap(),
            request
        );
        let mut output = Vec::new();
        let reply: serde_json::Value = client
            .response_stream(&serde_json::Value::Null, &mut output, 65536, 10)
            .await
            .unwrap();
        assert_eq!(reply, serde_json::json!({"accepted":true}));
        assert_eq!(output, vec![7; 65536]);
        listener.stop().await;
    }
    #[tokio::test]
    async fn oversized_or_revoked_streams_cannot_return_publication_permission() {
        for (revoke, maximum) in [(false, 100), (true, 65536)] {
            let (listener, client) = fixture(revoke).await;
            let mut output = Vec::new();
            assert!(client
                .response_stream::<_, serde_json::Value>(
                    &serde_json::Value::Null,
                    &mut output,
                    maximum,
                    10
                )
                .await
                .is_err());
            assert!(output.len() as u64 <= maximum);
            listener.stop().await;
        }
    }
}
