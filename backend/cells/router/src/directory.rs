//! One bounded directory exchange over the existing pinned node-key TLS transport.
pub use crate::rpc::Config;
use anyhow::Result;
use codeyard_db_directory::{Directory, Refusal};
use codeyard_model::cells::{Cell, Key, Location, Object};
use codeyard_support::{cells::Prefix, node_identity::NodeKey};
use iroh::PublicKey;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};

pub const ALPN: &[u8] = b"codeyard/directory/1";

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    NodeCell {
        public_key: String,
    },
    RouterKey {
        public_key: String,
    },
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
    Release { object: Object, keys: Vec<Key> },
    Forget { object: Object },
    ReportLoad {
        database_bytes: i64,
        write_rate: i64,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub enum Reply {
    RouterKey(bool),
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
pub struct Client(crate::rpc::Client);
impl Client {
    pub fn new(key: NodeKey, address: SocketAddr, peer: PublicKey, config: Config) -> Result<Self> {
        Ok(Self(crate::rpc::Client::new(
            key, address, peer, ALPN, config,
        )?))
    }
    pub async fn request(&self, request: &Request) -> Result<Reply> {
        match self.0.request(request).await? {
            Reply::Error(error) => Err(error.into()),
            reply => Ok(reply),
        }
    }
}
pub struct Listener {
    pub address: SocketAddr,
    inner: crate::rpc::Listener,
}
impl Listener {
    pub async fn bind(
        directory: Directory,
        key: &NodeKey,
        address: SocketAddr,
        config: Config,
    ) -> Result<Self> {
        let inner = crate::rpc::Listener::bind(
            Arc::new(DirectoryService {
                directory,
                maximum_bytes: config.maximum_bytes,
            }),
            key,
            address,
            ALPN,
            config,
        )
        .await?;
        Ok(Self {
            address: inner.address,
            inner,
        })
    }
    pub async fn ended(&mut self) {
        self.inner.ended().await;
    }
    pub async fn stop(self) {
        self.inner.stop().await;
    }
}
struct DirectoryService {
    directory: Directory,
    maximum_bytes: usize,
}
impl crate::rpc::Service for DirectoryService {
    fn admit<'a>(&'a self, peer: PublicKey) -> futures_util::future::BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.directory.authorize_read(&peer.to_string()).await })
    }
    fn handle<'a>(
        &'a self,
        peer: PublicKey,
        bytes: &'a [u8],
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let reply = match serde_json::from_slice::<Request>(bytes) {
                Ok(request) => match handle(&self.directory, &peer.to_string(), request).await {
                    Ok(reply) => reply,
                    Err(error) => Reply::Error(Failure::from_error(&error)),
                },
                Err(_) => Reply::Error(Failure::Invalid),
            };
            let bytes = serde_json::to_vec(&reply)?;
            if bytes.len() > self.maximum_bytes {
                return Ok(serde_json::to_vec(&Reply::Error(Failure::Unavailable))?);
            }
            Ok(bytes)
        })
    }
}

async fn handle(directory: &Directory, peer: &str, request: Request) -> Result<Reply> {
    // A router key is read-only. Mutations recheck cell enrollment and retain it
    // inside their committing transaction, so this read admission grants no writes.
    Ok(match request {
        Request::NodeCell { public_key } => {
            Reply::Prefix(directory.node_cell_for(peer, &public_key).await?)
        }
        Request::RouterKey { public_key } => {
            Reply::RouterKey(directory.router_key_for(peer, &public_key).await?)
        }
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
        Request::Release { object, keys } => {
            directory.release(peer, object, &keys).await?;
            Reply::Done
        }
        Request::Forget { object } => {
            directory.forget(peer, object).await?;
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

// Publication errors retain their domain kind without exposing transport or database details.
fn publication_error(error: anyhow::Error) -> anyhow::Error {
    use codeyard_db_core::cells::directory::Refusal;
    match error.downcast_ref::<Failure>() {
        Some(Failure::Conflict) => Refusal::Conflict.into(),
        Some(Failure::Denied) => Refusal::Denied.into(),
        _ => Refusal::Unavailable.into(),
    }
}
impl codeyard_db_core::cells::directory::Directory for Client {
    fn config(&self) -> codeyard_db_core::cells::directory::Config {
        codeyard_db_core::cells::directory::Config {
            request_seconds: self.0.config().exchange_seconds,
            ..Default::default()
        }
    }
    fn lookup<'a>(&'a self, key: &'a Key) -> futures_util::future::BoxFuture<'a, Result<Option<Location>>> {
        Box::pin(async move {
            match self.request(&Request::Lookup { key: key.clone() }).await.map_err(publication_error)? {
                Reply::Location(location) => Ok(location),
                _ => Err(codeyard_db_core::cells::directory::Refusal::Unavailable.into()),
            }
        })
    }
    fn reserve<'a>(&'a self, object: Object, keys: &'a [Key], nonce: &'a str) -> futures_util::future::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match self.request(&Request::Reserve { object, keys: keys.to_vec(), nonce: nonce.into() }).await.map_err(publication_error)? {
                Reply::Reserved => Ok(()),
                _ => Err(codeyard_db_core::cells::directory::Refusal::Unavailable.into()),
            }
        })
    }
    fn confirm<'a>(&'a self, object: Object, keys: &'a [Key], nonce: &'a str) -> futures_util::future::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match self.request(&Request::Confirm { object, keys: keys.to_vec(), nonce: nonce.into() }).await.map_err(publication_error)? {
                Reply::Done => Ok(()),
                _ => Err(codeyard_db_core::cells::directory::Refusal::Unavailable.into()),
            }
        })
    }
    fn release<'a>(&'a self, object: Object, keys: &'a [Key]) -> futures_util::future::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match self.request(&Request::Release { object, keys: keys.to_vec() }).await.map_err(publication_error)? {
                Reply::Done => Ok(()),
                _ => Err(codeyard_db_core::cells::directory::Refusal::Unavailable.into()),
            }
        })
    }
    fn forget(&self, object: Object) -> futures_util::future::BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            match self.request(&Request::Forget { object }).await.map_err(publication_error)? {
                Reply::Done => Ok(()),
                _ => Err(codeyard_db_core::cells::directory::Refusal::Unavailable.into()),
            }
        })
    }
}
