//! Foreign credential checks use the shared pinned node-key RPC transport.
use crate::{
    directory::{Client as Directory, Reply as DirectoryReply, Request as DirectoryRequest},
    routes::Routes,
    rpc,
};
use anyhow::{ensure, Result};
use codeyard_db_core::cells::authorization::{Answer, Home, Proof};
use codeyard_model::cells::{KeyKind, Object, ObjectKind};
use codeyard_support::node_identity::NodeKey;
use futures_util::future::BoxFuture;
use iroh::PublicKey;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::Semaphore;

pub const ALPN: &[u8] = b"codeyard/cells/1";
#[derive(Serialize, Deserialize)]
#[serde(
    tag = "result",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Reply {
    Account(Option<Box<Answer>>),
    Unavailable,
}
pub struct Remote {
    key: NodeKey,
    routes: Routes,
    slots: Arc<Semaphore>,
}
impl Remote {
    pub fn new(key: NodeKey, directory: Directory) -> Result<Self> {
        Ok(Self {
            key,
            routes: Routes::new(directory, Default::default())?,
            slots: Arc::new(Semaphore::new(64)),
        })
    }
}
impl Home for Remote {
    fn authorize<'a>(&'a self, proof: &'a Proof) -> BoxFuture<'a, Result<Option<Answer>>> {
        Box::pin(async move {
            proof.validate()?;
            let _slot = self.slots.clone().try_acquire_owned()?;
            let cell = match proof {
                Proof::Token { home, .. } | Proof::Session { home, .. } => {
                    self.routes.cell(*home).await?
                }
                Proof::Ssh { fingerprint } => self.routes.by_key(KeyKind::Ssh, fingerprint).await?,
            };
            let Some(cell) = cell else { return Ok(None) };
            ensure!(
                !cell.rpc_addresses.is_empty(),
                "The home cell has no RPC listener"
            );
            let address: SocketAddr = cell.rpc_addresses[0].parse()?;
            let mut answer = None;
            // A registry record can include several node keys and SSH host-key pins.
            // Only the node key that signs this pinned TLS connection is accepted.
            for public in &cell.node_keys {
                let Ok(public) = codeyard_support::node_identity::parse_public_key(public) else {
                    continue;
                };
                let client = rpc::Client::new(
                    self.key.clone(),
                    address,
                    public,
                    ALPN,
                    rpc::Config {
                        maximum_bytes: 128 * 1024,
                        ..Default::default()
                    },
                )?;
                match client.request::<_, Reply>(proof).await {
                    Ok(Reply::Account(value)) => {
                        answer = Some(value.map(|value| *value));
                        break;
                    }
                    Ok(Reply::Unavailable) => {
                        anyhow::bail!("Home-cell authorization is unavailable")
                    }
                    Err(_) => {}
                }
            }
            let answer = answer.ok_or_else(|| anyhow::anyhow!("The home cell is unavailable"))?;
            let Some(answer) = answer else {
                return Ok(None);
            };
            ensure!(
                answer.home == cell.id && answer.user.home_cell == i32::from(cell.id.get()),
                "Account answer has the wrong home"
            );
            let home = self
                .routes
                .by_object(Object {
                    kind: ObjectKind::User,
                    id: answer.user.id,
                })
                .await?;
            ensure!(
                home.is_some_and(|home| home.id == answer.home),
                "Account answer is not from its directory home"
            );
            Ok(Some(answer))
        })
    }
}
struct Accounts {
    directory: Directory,
    home: Arc<dyn Home>,
}
impl Accounts {
    async fn admitted(&self, peer: PublicKey) -> Result<()> {
        ensure!(
            matches!(
                self.directory
                    .request(&DirectoryRequest::NodeCell {
                        public_key: peer.to_string()
                    })
                    .await?,
                DirectoryReply::Prefix(_)
            ),
            "Caller is not an enrolled cell"
        );
        Ok(())
    }
}
impl rpc::Service for Accounts {
    fn admit<'a>(&'a self, peer: PublicKey) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.admitted(peer).await })
    }
    fn handle<'a>(&'a self, peer: PublicKey, bytes: &'a [u8]) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            self.admitted(peer).await?;
            let proof: Proof = serde_json::from_slice(bytes)?;
            proof.validate()?;
            let reply = match self.home.authorize(&proof).await {
                Ok(answer) => Reply::Account(answer.map(Box::new)),
                Err(_) => Reply::Unavailable,
            };
            self.admitted(peer).await?;
            Ok(serde_json::to_vec(&reply)?)
        })
    }
}
pub async fn listen(
    directory: Directory,
    home: Arc<dyn Home>,
    key: &NodeKey,
    address: SocketAddr,
) -> Result<rpc::Listener> {
    rpc::Listener::bind(
        Arc::new(Accounts { directory, home }),
        key,
        address,
        ALPN,
        rpc::Config {
            maximum_bytes: 128 * 1024,
            ..Default::default()
        },
    )
    .await
}
