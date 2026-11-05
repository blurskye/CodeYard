//! One bounded directory exchange over the existing pinned node-key TLS transport.
use anyhow::{ensure, Result};
use codeyard_cluster_core::{link::read_to_end_within, tcp, tls};
use codeyard_db_directory::{Directory, Refusal};
use codeyard_model::cells::{Cell, Key, Location, Object};
use codeyard_support::{cells::Prefix, node_identity::NodeKey};
use iroh::PublicKey;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{split, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};
use tokio_rustls::TlsAcceptor;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

pub const ALPN: &[u8] = b"codeyard/directory/1";

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
    fn validate(self) -> Result<()> {
        ensure!(
            (1..=60).contains(&self.exchange_seconds)
                && (1024..=16 * 1024 * 1024).contains(&self.maximum_bytes)
                && (1..=4096).contains(&self.maximum_connections),
            "Invalid directory transport limits"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    NextPrefix {
        previous: Prefix,
        last_id: i64,
    },
    Cell {
        prefix: Prefix,
    },
    Choose {
        region: String,
    },
    Lookup {
        key: Key,
    },
    Home {
        object: Object,
    },
    Reserve {
        object: Object,
        keys: Vec<Key>,
        nonce: String,
    },
    Confirm {
        object: Object,
        keys: Vec<Key>,
        nonce: String,
    },
    ReportLoad {
        database_bytes: i64,
        write_rate: i64,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub enum Reply {
    Prefix(Prefix),
    Cell(Option<Cell>),
    Location(Option<Location>),
    Home(Option<Prefix>),
    Reserved,
    Done,
    Error(Failure),
}
#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    #[error("Directory request is invalid")]
    Invalid,
    #[error("Directory key is already claimed")]
    Conflict,
    #[error("Directory access denied")]
    Denied,
    #[error("Directory is unavailable")]
    Unavailable,
}
impl Failure {
    fn from_error(error: &anyhow::Error) -> Self {
        match error.downcast_ref::<Refusal>() {
            Some(Refusal::Invalid) => Self::Invalid,
            Some(Refusal::Conflict) => Self::Conflict,
            Some(Refusal::Denied) => Self::Denied,
            _ => Self::Unavailable,
        }
    }
}

#[derive(Clone)]
pub struct Client {
    key: NodeKey,
    address: SocketAddr,
    peer: PublicKey,
    config: Config,
}
impl Client {
    pub fn new(key: NodeKey, address: SocketAddr, peer: PublicKey, config: Config) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            key,
            address,
            peer,
            config,
        })
    }
    pub async fn request(&self, request: &Request) -> Result<Reply> {
        let bytes = serde_json::to_vec(request)?;
        ensure!(bytes.len() <= self.config.maximum_bytes, Failure::Invalid);
        tokio::time::timeout(Duration::from_secs(self.config.exchange_seconds), async {
            let mut link = tcp::dial(&self.key, self.address, self.peer, ALPN).await?;
            link.writer.write_all(&bytes).await?;
            link.finish().await?;
            let bytes = read_to_end_within(&mut link.reader, self.config.maximum_bytes).await?;
            let reply: Reply = serde_json::from_slice(&bytes).map_err(|_| Failure::Unavailable)?;
            match reply {
                Reply::Error(error) => Err(error.into()),
                reply => Ok(reply),
            }
        })
        .await
        .map_err(|_| Failure::Unavailable)?
    }
}

/// The task owns every accepted connection. Dropping or stopping it closes them all.
pub struct Listener {
    pub address: SocketAddr,
    cancel: CancellationToken,
    task: Option<AbortOnDropHandle<()>>,
}
impl Listener {
    pub async fn bind(
        directory: Directory,
        key: &NodeKey,
        address: SocketAddr,
        config: Config,
    ) -> Result<Self> {
        config.validate()?;
        let acceptor = TlsAcceptor::from(tls::server_config_for(key, &[ALPN])?);
        let listener = TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let task = tokio::spawn(async move {
            let slots = Arc::new(Semaphore::new(config.maximum_connections));
            let mut tasks = JoinSet::new();
            loop {
                while tasks.try_join_next().is_some() {}
                let accepted = tokio::select! {
                    _ = stop.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let (socket, _) = match accepted {
                    Ok(socket) => socket,
                    Err(error) => {
                        tracing::warn!(%error, "directory listener failed");
                        break;
                    }
                };
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    continue;
                };
                tasks.spawn(serve(
                    socket,
                    slot,
                    acceptor.clone(),
                    directory.clone(),
                    config,
                ));
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
    /// Resolves if the listener fails or ends; the owner must stop serving too.
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

async fn serve(
    socket: TcpStream,
    _slot: OwnedSemaphorePermit,
    acceptor: TlsAcceptor,
    directory: Directory,
    config: Config,
) {
    tcp::tune(&socket);
    let _ = tokio::time::timeout(Duration::from_secs(config.exchange_seconds), async {
        let stream = acceptor.accept(socket).await?;
        ensure!(
            stream.get_ref().1.alpn_protocol() == Some(ALPN),
            Failure::Denied
        );
        let cert = stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|certs| certs.first())
            .ok_or(Failure::Denied)?;
        let peer = tls::peer_key(cert)?.to_string();
        // Authenticate before accepting any request body, including route lookups.
        directory.authorize_read(&peer).await?;
        let (mut reader, mut writer) = split(stream);
        let bytes = read_to_end_within(&mut reader, config.maximum_bytes).await?;
        let reply = match serde_json::from_slice::<Request>(&bytes) {
            Ok(request) => match handle(&directory, &peer, request).await {
                Ok(reply) => reply,
                Err(error) => Reply::Error(Failure::from_error(&error)),
            },
            Err(_) => Reply::Error(Failure::Invalid),
        };
        let mut bytes = serde_json::to_vec(&reply)?;
        if bytes.len() > config.maximum_bytes {
            bytes = serde_json::to_vec(&Reply::Error(Failure::Unavailable))?;
        }
        writer.write_all(&bytes).await?;
        writer.shutdown().await?;
        anyhow::Ok(())
    })
    .await;
}

async fn handle(directory: &Directory, peer: &str, request: Request) -> Result<Reply> {
    // A router key is read-only. Mutations recheck cell enrollment and retain it
    // inside their committing transaction, so this read admission grants no writes.
    Ok(match request {
        Request::NextPrefix { previous, last_id } => {
            Reply::Prefix(directory.next_prefix(peer, previous, last_id).await?)
        }
        Request::Cell { prefix } => Reply::Cell(directory.cell_for(Some(peer), prefix).await?),
        Request::Choose { region } => {
            Reply::Cell(directory.choose_cell_for(Some(peer), &region).await?)
        }
        Request::Lookup { key } => Reply::Location(directory.lookup_for(Some(peer), &key).await?),
        Request::Home { object } => {
            Reply::Home(directory.object_home_for(Some(peer), object).await?)
        }
        Request::Reserve {
            object,
            keys,
            nonce,
        } => {
            directory.reserve(peer, object, &keys, &nonce).await?;
            Reply::Reserved
        }
        Request::Confirm {
            object,
            keys,
            nonce,
        } => {
            directory.confirm(peer, object, &keys, &nonce).await?;
            Reply::Done
        }
        Request::ReportLoad {
            database_bytes,
            write_rate,
        } => {
            directory
                .report_load(peer, database_bytes, write_rate)
                .await?;
            Reply::Done
        }
    })
}
