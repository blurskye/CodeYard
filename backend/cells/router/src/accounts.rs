//! Foreign credential checks use the shared pinned node-key RPC transport.
use crate::{
    directory::{Client as Directory, Reply as DirectoryReply, Request as DirectoryRequest},
    routes::{ReadyCell, Routes},
    rpc,
};
use anyhow::{ensure, Result};
use codeyard_db_core::{
    cells::authorization::{Answer, Home, Proof},
    models::User,
};
use codeyard_model::cells::{Key, KeyKind, Object, ObjectKind};
use codeyard_support::node_identity::NodeKey;
use futures_util::future::BoxFuture;
use iroh::PublicKey;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::Semaphore;

pub const ALPN: &[u8] = b"codeyard/cells/1";
#[derive(Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Request {
    Authorize(Proof),
    User(String),
}
#[derive(Serialize, Deserialize)]
#[serde(
    tag = "result",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Reply {
    Account(Option<Box<Answer>>),
    User(Option<Box<User>>),
    Unavailable,
}
pub struct Remote {
    key: NodeKey,
    routes: Routes,
    directory: Directory,
    slots: Arc<Semaphore>,
}
impl Remote {
    pub fn new(key: NodeKey, directory: Directory) -> Result<Self> {
        Ok(Self {
            key,
            routes: Routes::new(directory.clone(), Default::default())?,
            directory,
            slots: Arc::new(Semaphore::new(64)),
        })
    }
}
impl Remote {
    async fn exchange(&self, cell: &ReadyCell, request: &Request) -> Result<Reply> {
        ensure!(
            !cell.rpc_addresses.is_empty(),
            "The home cell has no RPC listener"
        );
        let address: SocketAddr = cell.rpc_addresses[0].parse()?;
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
            match client.request::<_, Reply>(request).await {
                Ok(Reply::Unavailable) => anyhow::bail!("Home-cell authorization is unavailable"),
                Ok(reply) => return Ok(reply),
                Err(_) => {}
            }
        }
        anyhow::bail!("The home cell is unavailable")
    }
}
impl Home for Remote {
    fn user<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<User>>> {
        Box::pin(async move {
            let key = Key::new(KeyKind::Name, name)?;
            let _slot = self.slots.clone().try_acquire_owned()?;
            let location = match self
                .directory
                .request(&DirectoryRequest::Lookup { key })
                .await?
            {
                DirectoryReply::Location(location) => location,
                _ => anyhow::bail!("Invalid directory account answer"),
            };
            let Some(location) =
                location.filter(|location| location.object.kind == ObjectKind::User)
            else {
                return Ok(None);
            };
            let cell = self
                .routes
                .cell(location.cell)
                .await?
                .ok_or_else(|| anyhow::anyhow!("The home cell is unavailable"))?;
            let Reply::User(user) = self
                .exchange(&cell, &Request::User(name.to_owned()))
                .await?
            else {
                anyhow::bail!("Invalid home metadata answer");
            };
            let Some(user) = user else { return Ok(None) };
            ensure!(
                user.id == location.object.id
                    && user.username == name
                    && !user.is_stub
                    && user.home_cell == i32::from(cell.id.get()),
                "Metadata answer has the wrong home"
            );
            codeyard_db_core::cells::authorization::validate_public_user(&user)?;
            Ok(Some(*user))
        })
    }
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
            let Reply::Account(answer) = self
                .exchange(&cell, &Request::Authorize(proof.clone()))
                .await?
            else {
                anyhow::bail!("Invalid home authorization answer");
            };
            let answer = answer.map(|answer| *answer);
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
            let request: Request = serde_json::from_slice(bytes)?;
            let reply = match request {
                Request::Authorize(proof) => {
                    proof.validate()?;
                    match self.home.authorize(&proof).await {
                        Ok(answer) => Reply::Account(answer.map(Box::new)),
                        Err(_) => Reply::Unavailable,
                    }
                }
                Request::User(name) => {
                    Key::new(KeyKind::Name, &name)?;
                    match self.home.user(&name).await {
                        Ok(user) => Reply::User(user.map(Box::new)),
                        Err(_) => Reply::Unavailable,
                    }
                }
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
