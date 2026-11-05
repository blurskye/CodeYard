//! Bounded node-key TLS request/response transport shared by directory and cell RPC.
use anyhow::{ensure, Result};
use codeyard_cluster_core::{link::read_to_end_within, tcp, tls};
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
    let _ = tokio::time::timeout(Duration::from_secs(config.exchange_seconds), async {
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
        let (mut reader, mut writer) = split(stream);
        let bytes = read_to_end_within(&mut reader, config.maximum_bytes).await?;
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
