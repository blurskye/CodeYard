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
    UserId(i64),
    Membership(codeyard_db_core::cells::memberships::Event),
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
    Done,
    Unavailable,
}
pub struct Remote {
    key: NodeKey,
    routes: Routes,
    directory: Directory,
    slots: Arc<Semaphore>,
    http: crate::http::Transport,
}
impl Remote {
    pub fn new(key: NodeKey, directory: Directory) -> Result<Self> {
        Ok(Self {
            key,
            routes: Routes::new(directory.clone(), Default::default())?,
            directory,
            slots: Arc::new(Semaphore::new(64)),
            http: crate::http::Transport::new(crate::http::Config {
                maximum_connections: 64,
                maximum_request_bytes: 65536,
                maximum_response_bytes: 8 * 1024 * 1024,
                exchange_seconds: 10,
                stall_seconds: 10,
                minimum_rate: 1024,
            })?,
        })
    }
}
impl Remote {
    async fn metadata(
        &self,
        cell: &ReadyCell,
        request: &Request,
        id: i64,
        name: Option<&str>,
    ) -> Result<Option<User>> {
        let Reply::User(user) = self.exchange(cell, request).await? else {
            anyhow::bail!("Invalid home metadata answer");
        };
        let Some(user) = user else {
            return Ok(None);
        };
        ensure!(
            user.id == id
                && name.is_none_or(|name| user.username == name)
                && !user.is_stub
                && user.home_cell == i32::from(cell.id.get()),
            "Metadata answer has the wrong home"
        );
        codeyard_db_core::cells::authorization::validate_public_user(&user)?;
        Ok(Some(*user))
    }
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
    fn user_id<'a>(&'a self, id: i64) -> BoxFuture<'a, Result<Option<User>>> {
        Box::pin(async move {
            codeyard_support::cells::Prefix::for_id(id)?;
            let _slot = self.slots.clone().try_acquire_owned()?;
            let Some(cell) = self
                .routes
                .by_object(Object {
                    kind: ObjectKind::User,
                    id,
                })
                .await?
            else {
                return Ok(None);
            };
            self.metadata(&cell, &Request::UserId(id), id, None).await
        })
    }
    fn membership_page<'a>(
        &'a self,
        cell: codeyard_support::cells::Prefix,
        page: &'a codeyard_db_core::cells::memberships::Page,
        credential: &'a str,
    ) -> BoxFuture<'a, Result<Vec<serde_json::Value>>> {
        Box::pin(async move {
            use codeyard_db_core::cells::memberships::List;
            page.validate()?;
            ensure!(credential.len() <= 4096, "Invalid membership credential");
            let cell = self
                .routes
                .cell(cell)
                .await?
                .ok_or_else(|| anyhow::anyhow!("The resource cell is unavailable"))?;
            let (path, kind) = match page.list {
                List::Organizations => ("/api/v3/user/orgs", ObjectKind::Organization),
                List::GraphqlOrganizations => ("/api/graphql", ObjectKind::Organization),
                List::Repositories => ("/api/v3/user/repos", ObjectKind::Repository),
            };
            let request = {
                let mut query = url::form_urlencoded::Serializer::new(String::new());
                query.extend_pairs(
                    page.query
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_str())),
                );
                query
                    .append_pair("page", &page.number.to_string())
                    .append_pair("per_page", &page.size.to_string());
                let graphql = matches!(page.list, List::GraphqlOrganizations);
                let offset = (i64::from(page.number) - 1)
                    .checked_mul(i64::from(page.size))
                    .ok_or_else(|| anyhow::anyhow!("Invalid membership page"))?;
                let body = if graphql {
                    axum::body::Body::from(serde_json::to_vec(&serde_json::json!({
                        "query":"query($first:Int!,$after:String){viewer{organizations(first:$first,after:$after){nodes{__typename id databaseId login name url avatarUrl createdAt description viewerCanCreateProjects}}}}",
                        "variables":{"first":page.size,"after":if offset==0 {None} else {Some(codeyard_model::graphql_cursor::encode(offset-1))}}
                    }))?)
                } else {
                    axum::body::Body::empty()
                };
                axum::http::Request::builder()
                    .method(if graphql { "POST" } else { "GET" })
                    .uri(if graphql {
                        path.to_owned()
                    } else {
                        format!("{path}?{}", query.finish())
                    })
                    .header(
                        axum::http::header::AUTHORIZATION,
                        format!("Bearer {credential}"),
                    )
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(body)?
            };
            ensure!(
                crate::selection::Credential::from_headers(request.headers(), "session_id")?
                    .is_some(),
                "Invalid membership credential"
            );
            let response = self.http.forward(&cell, credential, request).await?;
            ensure!(
                response.status().is_success(),
                "Remote membership read is unavailable"
            );
            let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024).await?;
            let document: serde_json::Value = serde_json::from_slice(&bytes)?;
            let rows: Vec<serde_json::Value> = if matches!(page.list, List::GraphqlOrganizations) {
                ensure!(
                    document.get("errors").is_none(),
                    "Remote membership read is unavailable"
                );
                serde_json::from_value(
                    document["data"]["viewer"]["organizations"]["nodes"].clone(),
                )?
            } else {
                serde_json::from_value(document)?
            };
            ensure!(
                rows.len() <= usize::from(page.size),
                "Remote membership page exceeds its limit"
            );
            let mut seen = std::collections::HashSet::new();
            for row in &rows {
                let id = if matches!(page.list, List::GraphqlOrganizations) {
                    row["id"]
                        .as_str()
                        .and_then(codeyard_model::node_id::decode)
                        .filter(|(kind, _)| *kind == "O")
                        .map(|(_, id)| id)
                } else {
                    row["id"].as_i64()
                }
                .ok_or_else(|| anyhow::anyhow!("Invalid membership document"))?;
                ensure!(seen.insert(id), "Duplicate membership document");
                let home = self.routes.by_object(Object { kind, id }).await?;
                ensure!(
                    home.is_some_and(|home| home.id == cell.id),
                    "Membership document has another home"
                );
            }
            Ok(rows)
        })
    }
    fn send_membership<'a>(
        &'a self,
        event: &'a codeyard_db_core::cells::memberships::Event,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            event.validate()?;
            let _slot = self.slots.clone().try_acquire_owned()?;
            let cell = self
                .routes
                .by_object(Object {
                    kind: ObjectKind::User,
                    id: event.change.user_id,
                })
                .await?
                .ok_or_else(|| anyhow::anyhow!("The account home is unavailable"))?;
            ensure!(
                matches!(
                    self.exchange(&cell, &Request::Membership(event.clone()))
                        .await?,
                    Reply::Done
                ),
                "Membership delivery is unavailable"
            );
            Ok(())
        })
    }
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
            self.metadata(
                &cell,
                &Request::User(name.to_owned()),
                location.object.id,
                Some(name),
            )
            .await
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
async fn enrolled(
    directory: &Directory,
    peer: PublicKey,
) -> Result<codeyard_support::cells::Prefix> {
    match directory
        .request(&DirectoryRequest::NodeCell {
            public_key: peer.to_string(),
        })
        .await?
    {
        DirectoryReply::Prefix(cell) => Ok(cell),
        _ => anyhow::bail!("Caller is not an enrolled cell"),
    }
}
struct MembershipAdmission {
    directory: Directory,
    peer: PublicKey,
    recipient: codeyard_support::cells::Prefix,
}
impl codeyard_db_core::cells::memberships::Admission for MembershipAdmission {
    fn check<'a>(
        &'a self,
        event: &'a codeyard_db_core::cells::memberships::Event,
    ) -> BoxFuture<'a, Result<codeyard_support::cells::Prefix>> {
        Box::pin(async move {
            event.validate()?;
            let resource_request = DirectoryRequest::Home {
                object: event.change.object,
            };
            let recipient_request = DirectoryRequest::Home {
                object: Object {
                    kind: ObjectKind::User,
                    id: event.change.user_id,
                },
            };
            let issuer_request = DirectoryRequest::Cell {
                prefix: codeyard_support::cells::Prefix::for_id(event.id)?,
            };
            let (source, resource, recipient, issuer) = tokio::try_join!(
                enrolled(&self.directory, self.peer),
                self.directory.request(&resource_request),
                self.directory.request(&recipient_request),
                self.directory.request(&issuer_request)
            )?;
            ensure!(
                matches!(resource,DirectoryReply::Home(Some(home)) if home == source)
                    && matches!(recipient,DirectoryReply::Home(Some(home)) if home == self.recipient)
                    && matches!(issuer,DirectoryReply::Cell(Some(cell)) if cell.id == source),
                "Membership source or recipient has another home"
            );
            Ok(source)
        })
    }
}
struct Accounts {
    directory: Directory,
    home: Arc<dyn Home>,
}
impl Accounts {
    async fn admitted(&self, peer: PublicKey) -> Result<()> {
        enrolled(&self.directory, peer).await.map(|_| ())
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
                Request::Membership(event) => {
                    let admission = MembershipAdmission {
                        directory: self.directory.clone(),
                        peer,
                        recipient: self
                            .home
                            .cell()
                            .ok_or_else(|| anyhow::anyhow!("No native home"))?,
                    };
                    match self.home.receive_membership(&event, &admission).await {
                        Ok(()) => Reply::Done,
                        Err(_) => Reply::Unavailable,
                    }
                }
                Request::Authorize(proof) => {
                    proof.validate()?;
                    match self.home.authorize(&proof).await {
                        Ok(answer) => Reply::Account(answer.map(Box::new)),
                        Err(_) => Reply::Unavailable,
                    }
                }
                Request::UserId(id) => {
                    codeyard_support::cells::Prefix::for_id(id)?;
                    match self.home.user_id(id).await {
                        Ok(user) => Reply::User(user.map(Box::new)),
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
