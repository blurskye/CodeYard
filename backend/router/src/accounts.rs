//! Foreign credential checks use the shared pinned node-key RPC transport.
use crate::{
    directory::{Client as Directory, Reply as DirectoryReply, Request as DirectoryRequest},
    routes::{ReadyCell, Routes},
    rpc,
};
use anyhow::{ensure, Result};
use codeyard_database::{
    cells::authorization::{Answer, Home, Proof},
    models::User,
};
use codeyard_security::node_identity::NodeKey;
use codeyard_types::cells::{Key, KeyKind, Object, ObjectKind};
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
    RepositorySource {
        owner: String,
        name: String,
        proof: Proof,
    },
    RepositoryId {
        id: i64,
        proof: Proof,
    },
    RepositoryFork {
        owner: String,
        base: i64,
        proof: Proof,
    },
    Snapshot(codeyard_database::cells::repositories::SnapshotRequest),
    Pointer(codeyard_database::cells::pointers::Event),
    Notice(Box<codeyard_database::cells::notices::Event>),
    NoticePublic(i64),
    Push(codeyard_database::cells::pulls::Event),
    Subscribe {
        binding: codeyard_database::cells::pulls::Binding,
        proof: Proof,
    },
    PullGrant {
        binding: codeyard_database::cells::pulls::Binding,
        proof: Option<Proof>,
    },
    PullSnapshot(codeyard_database::cells::pulls::HeadRequest),
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
    RepositorySource(Option<Box<codeyard_database::cells::repositories::Source>>),
    Snapshot(codeyard_database::cells::repositories::Snapshot),
    PullSnapshot(Option<codeyard_database::cells::repositories::Snapshot>),
    PullGrant(Option<codeyard_database::cells::pulls::Grant>),
    Done,
    NoticePublic(bool),
    Unavailable,
}
pub struct Remote {
    key: NodeKey,
    routes: Routes,
    directory: Directory,
    slots: Arc<Semaphore>,
    http: crate::http::Transport,
    forks: crate::http::Transport,
}
impl Remote {
    pub fn new(key: NodeKey, directory: Directory) -> Result<Self> {
        Ok(Self {
            key,
            routes: Routes::new(directory.clone(), Default::default())?,
            directory,
            slots: Arc::new(Semaphore::new(64)),
            forks: crate::http::Transport::new(crate::http::Config {
                maximum_connections: 8,
                maximum_request_bytes: 65536,
                maximum_response_bytes: 8 * 1024 * 1024,
                exchange_seconds: 600,
                stall_seconds: 30,
                minimum_rate: 1024,
            })?,
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
    async fn stream(
        &self,
        cell: &ReadyCell,
        request: &Request,
        reader: i64,
        output: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
        maximum: u64,
        seconds: u64,
    ) -> Result<Reply> {
        let address = cell
            .rpc_addresses
            .first()
            .ok_or_else(|| anyhow::anyhow!("Source has no RPC listener"))?
            .parse()?;
        for public in &cell.node_keys {
            let Ok(public) = codeyard_security::node_identity::parse_public_key(public) else {
                continue;
            };
            let candidate = rpc::Client::new(
                self.key.clone(),
                address,
                public,
                ALPN,
                rpc::Config {
                    maximum_bytes: 128 * 1024,
                    ..Default::default()
                },
            )?;
            // Select a live pinned key before writing anything into the owned pack.
            if !matches!(
                candidate
                    .request::<_, Reply>(&Request::UserId(reader))
                    .await,
                Ok(Reply::User(_))
            ) {
                continue;
            }
            return candidate
                .response_stream(request, output, maximum, seconds)
                .await;
        }
        anyhow::bail!("Source is unavailable")
    }
    async fn roots(&self, source: Object, target: Object) -> Result<(ReadyCell, ReadyCell)> {
        source.validate()?;
        target.validate()?;
        let (source, target) =
            tokio::try_join!(self.routes.by_object(source), self.routes.by_object(target))?;
        Ok((
            source.ok_or_else(|| anyhow::anyhow!("Source home is unavailable"))?,
            target.ok_or_else(|| anyhow::anyhow!("Target home is unavailable"))?,
        ))
    }
    async fn source(
        &self,
        proof: &Proof,
        request: Request,
    ) -> Result<Option<codeyard_database::cells::repositories::Source>> {
        tokio::time::timeout(
            std::time::Duration::from_secs(rpc::Config::default().exchange_seconds),
            async move {
                let (owner, name, base, id) = match &request {
                    Request::RepositorySource { owner, name, .. } => {
                        ensure!(
                            codeyard_types::repository_names::valid(name),
                            "Invalid fork source name"
                        );
                        (Some(owner.as_str()), Some(name.as_str()), None, None)
                    }
                    Request::RepositoryFork { owner, base, .. } => {
                        codeyard_types::cell_ids::Prefix::for_id(*base)?;
                        (Some(owner.as_str()), None, Some(*base), None)
                    }
                    Request::RepositoryId { id, .. } => {
                        codeyard_types::cell_ids::Prefix::for_id(*id)?;
                        (None, None, None, Some(*id))
                    }
                    _ => anyhow::bail!("Invalid repository metadata request"),
                };
                proof.validate()?;
                let _slot = self.slots.clone().try_acquire_owned()?;
                let cell = if let Some(owner) = owner {
                    Key::new(KeyKind::Name, owner)?;
                    self.routes.by_name(owner).await?
                } else if let Some(id) = id {
                    self.routes
                        .by_object(Object {
                            kind: ObjectKind::Repository,
                            id,
                        })
                        .await?
                } else {
                    None
                };
                let Some(cell) = cell else {
                    return Ok(None);
                };
                let Reply::RepositorySource(source) = self.exchange(&cell, &request).await? else {
                    anyhow::bail!("Invalid fork source answer");
                };
                let Some(source) = source else {
                    return Ok(None);
                };
                let source = *source;
                source.validate(cell.id)?;
                ensure!(
                    owner.is_none_or(|owner| source.owner.name() == owner)
                        && id.is_none_or(|id| source.repository.id == id)
                        && name
                            .is_none_or(|name| source.repository.name.eq_ignore_ascii_case(name))
                        && base.is_none_or(|base| source.repository.forked_from_id == Some(base)),
                    "Fork source coordinates changed"
                );
                for object in [
                    source.owner.object(),
                    Object {
                        kind: ObjectKind::Repository,
                        id: source.repository.id,
                    },
                ] {
                    ensure!(
                        self.routes
                            .by_object(object)
                            .await?
                            .is_some_and(|home| home.id == cell.id),
                        "Fork source has another home"
                    );
                }
                Ok(Some(source))
            },
        )
        .await?
    }
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
        codeyard_database::cells::authorization::validate_public_user(&user)?;
        Ok(Some(*user))
    }
    async fn exchange(&self, cell: &ReadyCell, request: &Request) -> Result<Reply> {
        ensure!(
            !cell.rpc_addresses.is_empty(),
            "The home cell has no RPC listener"
        );
        let address: SocketAddr = cell.rpc_addresses[0].parse()?;
        for public in &cell.node_keys {
            let Ok(public) = codeyard_security::node_identity::parse_public_key(public) else {
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
    fn send_push<'a>(
        &'a self,
        event: &'a codeyard_database::cells::pulls::Event,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            event.validate()?;
            let _slot = self.slots.clone().try_acquire_owned()?;
            // A deleted resource has no consumer; a directory outage still retries.
            let (source, target) = tokio::try_join!(
                self.routes.by_object(event.change.head()),
                self.routes.by_object(event.change.base())
            )?;
            let (Some(source), Some(target)) = (source, target) else {
                return Ok(());
            };
            let issuer = self
                .routes
                .cell(codeyard_types::cell_ids::Prefix::for_id(event.id)?)
                .await?;
            if issuer.is_none_or(|issuer| issuer.id != source.id) {
                return Ok(());
            }
            ensure!(
                matches!(
                    self.exchange(&target, &Request::Push(event.clone()))
                        .await?,
                    Reply::Done
                ),
                "Push delivery is unavailable"
            );
            Ok(())
        })
    }
    fn send_pull<'a>(
        &'a self,
        binding: &'a codeyard_database::cells::pulls::Binding,
        proof: &'a Proof,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            binding.validate()?;
            proof.validate()?;
            let _slot = self.slots.clone().try_acquire_owned()?;
            let (_, target) = self.roots(binding.base(), binding.head()).await?;
            ensure!(
                matches!(
                    self.exchange(
                        &target,
                        &Request::Subscribe {
                            binding: binding.clone(),
                            proof: proof.clone()
                        }
                    )
                    .await?,
                    Reply::Done
                ),
                "Pull subscription is unavailable"
            );
            Ok(())
        })
    }
    fn pull_grant<'a>(
        &'a self,
        binding: &'a codeyard_database::cells::pulls::Binding,
        proof: Option<&'a Proof>,
    ) -> BoxFuture<'a, Result<Option<codeyard_database::cells::pulls::Grant>>> {
        Box::pin(async move {
            binding.validate()?;
            if let Some(proof) = proof {
                proof.validate()?;
            }
            let _slot = self.slots.clone().try_acquire_owned()?;
            let (source, target) = self.roots(binding.head(), binding.base()).await?;
            let Reply::PullGrant(grant) = self
                .exchange(
                    &target,
                    &Request::PullGrant {
                        binding: binding.clone(),
                        proof: proof.cloned(),
                    },
                )
                .await?
            else {
                anyhow::bail!("Pull authority is unavailable");
            };
            if let Some(grant) = &grant {
                grant.validate(binding)?;
            }
            let (current_source, current_target) =
                self.roots(binding.head(), binding.base()).await?;
            ensure!(
                source.id == current_source.id && target.id == current_target.id,
                "Pull homes changed"
            );
            Ok(grant)
        })
    }
    fn pull_snapshot<'a>(
        &'a self,
        request: &'a codeyard_database::cells::pulls::HeadRequest,
        output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> BoxFuture<'a, Result<Option<codeyard_database::cells::repositories::Snapshot>>> {
        Box::pin(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(request.timeout_seconds),
                async {
                    request.validate()?;
                    let _slot = self.slots.clone().try_acquire_owned()?;
                    let (source, target) = self
                        .roots(request.binding.base(), request.binding.head())
                        .await?;
                    let Reply::PullSnapshot(snapshot) = self
                        .stream(
                            &target,
                            &Request::PullSnapshot(request.clone()),
                            request.publisher_id,
                            output,
                            request.maximum_bytes,
                            request.timeout_seconds,
                        )
                        .await?
                    else {
                        anyhow::bail!("Invalid pull snapshot answer");
                    };
                    if let Some(snapshot) = &snapshot {
                        request.validate_snapshot(snapshot)?;
                    }
                    let (current_source, current_target) = self
                        .roots(request.binding.base(), request.binding.head())
                        .await?;
                    ensure!(
                        source.id == current_source.id && target.id == current_target.id,
                        "Pull homes changed"
                    );
                    Ok(snapshot)
                },
            )
            .await?
        })
    }
    fn forward_fork<'a>(
        &'a self,
        owner: &'a str,
        request: &'a codeyard_database::cells::repositories::ForwardFork,
    ) -> BoxFuture<'a, Result<Option<codeyard_database::cells::repositories::ForwardedFork>>> {
        Box::pin(async move {
            request.validate()?;
            let key = Key::new(KeyKind::Name, owner)?;
            let Some(location) = self.routes.location(&key).await? else {
                return Ok(None);
            };
            if location.object.kind != request.target_kind {
                return Ok(None);
            }
            let cell = self
                .routes
                .cell(location.cell)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Fork destination is unavailable"))?;
            let mut http = axum::http::Request::builder()
                .method("POST")
                .uri(&request.path);
            for (name, value) in &request.headers {
                http = http.header(name.as_str(), value.as_str());
            }
            let mut http = http.body(axum::body::Body::from(request.body.clone()))?;
            http.headers_mut().insert(
                axum::http::header::ACCEPT_ENCODING,
                axum::http::HeaderValue::from_static("identity"),
            );
            let credential =
                crate::selection::Credential::from_headers(http.headers(), "session_id")?
                    .ok_or_else(|| anyhow::anyhow!("Fork forwarding requires authentication"))?;
            let response = self.forks.forward(&cell, credential.value(), http).await?;
            let status = response.status().as_u16();
            ensure!(
                response.headers().len() <= 128,
                "Fork response headers exceed their limit"
            );
            let mut header_bytes = 0_usize;
            let mut headers = Vec::new();
            for (name, value) in response.headers().iter() {
                header_bytes = header_bytes
                    .checked_add(name.as_str().len() + value.as_bytes().len())
                    .ok_or_else(|| anyhow::anyhow!("Fork response headers exceed their limit"))?;
                ensure!(
                    header_bytes <= 32768,
                    "Fork response headers exceed their limit"
                );
                headers.push((name.as_str().to_owned(), value.to_str()?.to_owned()));
            }
            let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
                .await?
                .to_vec();
            Ok(Some(
                codeyard_database::cells::repositories::ForwardedFork {
                    status,
                    headers,
                    body,
                },
            ))
        })
    }
    fn snapshot<'a>(
        &'a self,
        request: &'a codeyard_database::cells::repositories::SnapshotRequest,
        output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> BoxFuture<'a, Result<codeyard_database::cells::repositories::Snapshot>> {
        Box::pin(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(request.timeout_seconds),
                async move {
                    request.validate()?;
                    let _slot = self.slots.clone().try_acquire_owned()?;
                    let cell = self
                        .routes
                        .by_name(&request.owner)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("Fork source is unavailable"))?;
                    ensure!(
                        self.routes
                            .by_object(Object {
                                kind: ObjectKind::Repository,
                                id: request.repository_id
                            })
                            .await?
                            .is_some_and(|home| home.id == cell.id),
                        "Fork source has another home"
                    );
                    let Reply::Snapshot(snapshot) = self
                        .stream(
                            &cell,
                            &Request::Snapshot(request.clone()),
                            request.reader_id,
                            output,
                            request.maximum_bytes,
                            request.timeout_seconds,
                        )
                        .await?
                    else {
                        anyhow::bail!("Invalid fork snapshot answer");
                    };
                    snapshot.validate(request)?;
                    ensure!(
                        self.routes
                            .by_object(Object {
                                kind: ObjectKind::Repository,
                                id: request.repository_id
                            })
                            .await?
                            .is_some_and(|home| home.id == cell.id),
                        "Fork source home changed"
                    );
                    Ok(snapshot)
                },
            )
            .await?
        })
    }
    fn repository_source<'a>(
        &'a self,
        owner: &'a str,
        name: &'a str,
        proof: &'a Proof,
    ) -> BoxFuture<'a, Result<Option<codeyard_database::cells::repositories::Source>>> {
        Box::pin(async move {
            self.source(
                proof,
                Request::RepositorySource {
                    owner: owner.to_owned(),
                    name: name.to_owned(),
                    proof: proof.clone(),
                },
            )
            .await
        })
    }
    fn repository_id<'a>(
        &'a self,
        id: i64,
        proof: &'a Proof,
    ) -> BoxFuture<'a, Result<Option<codeyard_database::cells::repositories::Source>>> {
        Box::pin(async move {
            self.source(
                proof,
                Request::RepositoryId {
                    id,
                    proof: proof.clone(),
                },
            )
            .await
        })
    }
    fn repository_fork<'a>(
        &'a self,
        owner: &'a str,
        base: i64,
        proof: &'a Proof,
    ) -> BoxFuture<'a, Result<Option<codeyard_database::cells::repositories::Source>>> {
        Box::pin(async move {
            self.source(
                proof,
                Request::RepositoryFork {
                    owner: owner.to_owned(),
                    base,
                    proof: proof.clone(),
                },
            )
            .await
        })
    }
    fn user_id<'a>(&'a self, id: i64) -> BoxFuture<'a, Result<Option<User>>> {
        Box::pin(async move {
            codeyard_types::cell_ids::Prefix::for_id(id)?;
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
    fn resource_page<'a>(
        &'a self,
        cell: codeyard_types::cell_ids::Prefix,
        page: &'a codeyard_database::cells::pointers::Page,
        credential: Option<&'a str>,
    ) -> BoxFuture<'a, Result<codeyard_database::cells::pointers::PageResult>> {
        Box::pin(async move {
            use codeyard_database::cells::pointers::List;
            page.validate()?;
            ensure!(
                credential.is_none_or(|value| value.len() <= 4096)
                    && (page.list.stars() || credential.is_some()),
                "Invalid resource credential"
            );
            let cell = self
                .routes
                .cell(cell)
                .await?
                .ok_or_else(|| anyhow::anyhow!("The resource cell is unavailable"))?;
            let username = page
                .query
                .iter()
                .find(|(key, _)| key == "username")
                .map(|(_, value)| value.as_str())
                .unwrap_or("");
            let encoded: String =
                url::form_urlencoded::byte_serialize(username.as_bytes()).collect();
            let star_path = if matches!(page.list, List::BrowserStars) {
                format!("/api/users/{encoded}/starred")
            } else {
                format!("/api/v3/users/{encoded}/starred")
            };
            let (path, kind) = match page.list {
                List::Organizations => ("/api/v3/user/orgs", ObjectKind::Organization),
                List::GraphqlOrganizations => ("/api/graphql", ObjectKind::Organization),
                List::Repositories => ("/api/v3/user/repos", ObjectKind::Repository),
                List::BrowserRepositories => ("/", ObjectKind::Repository),
                List::BrowserOrganizations => ("/settings/organizations", ObjectKind::Organization),
                List::StarredRepositories | List::BrowserStars => {
                    (star_path.as_str(), ObjectKind::Repository)
                }
            };
            let request = {
                let mut query = url::form_urlencoded::Serializer::new(String::new());
                query.extend_pairs(
                    page.query
                        .iter()
                        .filter(|(key, _)| key != "username")
                        .map(|(key, value)| (key.as_str(), value.as_str())),
                );
                query
                    .append_pair("page", &page.number.to_string())
                    .append_pair("per_page", &page.size.to_string());
                let graphql = matches!(page.list, List::GraphqlOrganizations);
                let offset = (i64::from(page.number) - 1)
                    .checked_mul(i64::from(page.size))
                    .ok_or_else(|| anyhow::anyhow!("Invalid resource page"))?;
                let body = if graphql {
                    axum::body::Body::from(serde_json::to_vec(&serde_json::json!({
                        "query":"query($first:Int!,$after:String){viewer{organizations(first:$first,after:$after){nodes{__typename id databaseId login name url avatarUrl createdAt description viewerCanCreateProjects}}}}",
                        "variables":{"first":page.size,"after":if offset==0 {None} else {Some(codeyard_types::graphql_cursor::encode(offset-1))}}
                    }))?)
                } else {
                    axum::body::Body::empty()
                };
                let browser = page.list.browser();
                let mut request = axum::http::Request::builder()
                    .method(if graphql { "POST" } else { "GET" })
                    .uri(if graphql {
                        path.to_owned()
                    } else {
                        format!("{path}?{}", query.finish())
                    })
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::ACCEPT_ENCODING, "identity");
                if let Some(credential) = credential {
                    if browser {
                        codeyard_types::cell_ids::Secret::cookie(credential)?;
                        request = request.header(
                            axum::http::header::COOKIE,
                            format!("session_id={credential}"),
                        );
                    } else {
                        request = request.header(
                            axum::http::header::AUTHORIZATION,
                            format!("Bearer {credential}"),
                        );
                    }
                }
                request = request.header(
                    axum::http::header::ACCEPT,
                    if browser {
                        "application/vnd.codeyard.page+json"
                    } else if page.list.stars() {
                        "application/vnd.github.star+json"
                    } else {
                        "application/json"
                    },
                );
                request.body(body)?
            };
            ensure!(
                crate::selection::Credential::from_headers(request.headers(), "session_id")?
                    .is_some()
                    == credential.is_some(),
                "Invalid resource credential"
            );
            let response = self
                .http
                .forward(&cell, credential.unwrap_or(""), request)
                .await?;
            ensure!(
                response.status().is_success(),
                "Remote resource read is unavailable"
            );
            let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024).await?;
            let document: serde_json::Value = serde_json::from_slice(&bytes)?;
            let total = if matches!(page.list, List::BrowserRepositories) {
                ensure!(
                    document["page"]["lane"] == "Discovery",
                    "Invalid browser repository page"
                );
                Some(
                    document["page"]["model"]["Home"]["shared_count"]
                        .as_i64()
                        .filter(|count| *count >= 0)
                        .ok_or_else(|| anyhow::anyhow!("Invalid resource count"))?,
                )
            } else if matches!(page.list, List::BrowserStars) {
                let count = document["total"].as_i64();
                ensure!(
                    page.number != 1 || count.is_some_and(|count| count >= 0),
                    "Invalid star count"
                );
                count
            } else {
                None
            };
            let rows: Vec<serde_json::Value> = if matches!(page.list, List::BrowserRepositories) {
                serde_json::from_value(document["page"]["model"]["Home"]["shared"].clone())?
            } else if matches!(page.list, List::BrowserStars) {
                serde_json::from_value(document["rows"].clone())?
            } else if matches!(page.list, List::BrowserOrganizations) {
                ensure!(
                    document["page"]["lane"] == "Organizations",
                    "Invalid browser organization page"
                );
                serde_json::from_value(document["page"]["model"]["organizations"].clone())?
            } else if matches!(page.list, List::GraphqlOrganizations) {
                ensure!(
                    document.get("errors").is_none(),
                    "Remote resource read is unavailable"
                );
                serde_json::from_value(
                    document["data"]["viewer"]["organizations"]["nodes"].clone(),
                )?
            } else {
                serde_json::from_value(document)?
            };
            ensure!(
                rows.len() <= usize::from(page.size),
                "Remote resource page exceeds its limit"
            );
            let mut seen = std::collections::HashSet::new();
            for row in &rows {
                let id = if matches!(page.list, List::GraphqlOrganizations) {
                    row["id"]
                        .as_str()
                        .and_then(codeyard_types::node_id::decode)
                        .filter(|(kind, _)| *kind == "O")
                        .map(|(_, id)| id)
                } else if page.list.stars() {
                    row["repo"]["id"].as_i64()
                } else if matches!(page.list, List::BrowserRepositories) {
                    row["repository"]["id"].as_i64()
                } else {
                    row["id"].as_i64()
                }
                .ok_or_else(|| anyhow::anyhow!("Invalid resource document"))?;
                ensure!(seen.insert(id), "Duplicate resource document");
                let home = self.routes.by_object(Object { kind, id }).await?;
                ensure!(
                    home.is_some_and(|home| home.id == cell.id),
                    "Resource document has another home"
                );
            }
            Ok(codeyard_database::cells::pointers::PageResult { rows, total })
        })
    }
    fn send_pointer<'a>(
        &'a self,
        event: &'a codeyard_database::cells::pointers::Event,
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
                    self.exchange(&cell, &Request::Pointer(event.clone()))
                        .await?,
                    Reply::Done
                ),
                "Pointer delivery is unavailable"
            );
            Ok(())
        })
    }
    fn notice_public(&self, repository: i64) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async move {
            let _slot = self.slots.clone().try_acquire_owned()?;
            let object = Object {
                kind: ObjectKind::Repository,
                id: repository,
            };
            object.validate()?;
            let Some(cell) = self.routes.by_object(object).await? else {
                return Ok(false);
            };
            match self
                .exchange(&cell, &Request::NoticePublic(repository))
                .await?
            {
                Reply::NoticePublic(public) => Ok(public),
                _ => anyhow::bail!("Notice visibility is unavailable"),
            }
        })
    }
    fn send_notice<'a>(
        &'a self,
        event: &'a codeyard_database::cells::notices::Event,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _slot = self.slots.clone().try_acquire_owned()?;
            // A deleted account has no inbox; a directory outage still retries.
            let Some(cell) = self.routes.by_object(event.notice.recipient()).await? else {
                return Ok(());
            };
            ensure!(
                matches!(
                    self.exchange(&cell, &Request::Notice(Box::new(event.clone())))
                        .await?,
                    Reply::Done
                ),
                "Notice delivery is unavailable"
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
                Proof::Token { home, hash } => self.routes.token_home(*home, hash).await?,
                Proof::Session { home, .. } => self.routes.cell(*home).await?,
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
) -> Result<codeyard_types::cell_ids::Prefix> {
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
struct NoticeAdmission {
    directory: Directory,
    peer: PublicKey,
    recipient: codeyard_types::cell_ids::Prefix,
}
impl codeyard_database::cells::notices::Admission for NoticeAdmission {
    fn check<'a>(
        &'a self,
        event: &'a codeyard_database::cells::notices::Event,
    ) -> BoxFuture<'a, Result<codeyard_types::cell_ids::Prefix>> {
        Box::pin(async move {
            if event.withdrawn {
                // A deleted source has no directory home. Withdrawal can only narrow an
                // existing stub owned by this authenticated cell, never insert metadata.
                let caller = enrolled(&self.directory, self.peer).await?;
                event.validate(caller)?;
                ensure!(
                    matches!(self.directory.request(&DirectoryRequest::Home {
                    object: event.notice.recipient(),
                }).await?, DirectoryReply::Home(Some(home)) if home == self.recipient),
                    "Notice recipient has another home"
                );
                return Ok(caller);
            }
            roots(
                &self.directory,
                self.peer,
                self.recipient,
                event.notice.repository_object(),
                event.notice.recipient(),
                Some(event.id),
            )
            .await
        })
    }
}
struct PointerAdmission {
    directory: Directory,
    peer: PublicKey,
    recipient: codeyard_types::cell_ids::Prefix,
}
impl codeyard_database::cells::pointers::Admission for PointerAdmission {
    fn check<'a>(
        &'a self,
        event: &'a codeyard_database::cells::pointers::Event,
    ) -> BoxFuture<'a, Result<codeyard_types::cell_ids::Prefix>> {
        Box::pin(async move {
            event.validate()?;
            roots(
                &self.directory,
                self.peer,
                self.recipient,
                event.change.object,
                Object {
                    kind: ObjectKind::User,
                    id: event.change.user_id,
                },
                Some(event.id),
            )
            .await
        })
    }
}
async fn roots(
    directory: &Directory,
    peer: PublicKey,
    recipient: codeyard_types::cell_ids::Prefix,
    source: Object,
    target: Object,
    event: Option<i64>,
) -> Result<codeyard_types::cell_ids::Prefix> {
    source.validate()?;
    target.validate()?;
    let resource_request = DirectoryRequest::Home { object: source };
    let recipient_request = DirectoryRequest::Home { object: target };
    let (caller, resource, target) = tokio::try_join!(
        enrolled(directory, peer),
        directory.request(&resource_request),
        directory.request(&recipient_request)
    )?;
    ensure!(
        matches!(resource,DirectoryReply::Home(Some(home)) if home==caller)
            && matches!(target,DirectoryReply::Home(Some(home)) if home==recipient),
        "Source or recipient has another home"
    );
    if let Some(id) = event {
        ensure!(
            matches!(directory.request(&DirectoryRequest::Cell {prefix:codeyard_types::cell_ids::Prefix::for_id(id)?}).await?,DirectoryReply::Cell(Some(cell)) if cell.id==caller),
            "Event issuer has another home"
        );
    }
    Ok(caller)
}
struct PullAdmission {
    directory: Directory,
    peer: PublicKey,
    recipient: codeyard_types::cell_ids::Prefix,
}
impl codeyard_database::cells::pulls::Admission for PullAdmission {
    fn check<'a>(
        &'a self,
        source: Object,
        target: Object,
        event: Option<i64>,
    ) -> BoxFuture<'a, Result<codeyard_types::cell_ids::Prefix>> {
        Box::pin(async move {
            roots(
                &self.directory,
                self.peer,
                self.recipient,
                source,
                target,
                event,
            )
            .await
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
    fn streams_reply(&self, bytes: &[u8]) -> bool {
        matches!(
            serde_json::from_slice::<Request>(bytes),
            Ok(Request::Snapshot(_) | Request::PullSnapshot(_))
        )
    }
    fn stream_reply<'a>(
        &'a self,
        peer: PublicKey,
        bytes: &'a [u8],
        output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(codeyard_database::cells::authorization::scope(async move {
            use codeyard_nodes::link::write_frame;
            use tokio::io::AsyncReadExt;
            self.admitted(peer).await?;
            let request: Request = serde_json::from_slice(bytes)?;
            let maximum = match &request {
                Request::Snapshot(request) => {
                    request.validate()?;
                    request.maximum_bytes
                }
                Request::PullSnapshot(request) => {
                    request.validate()?;
                    request.maximum_bytes
                }
                _ => anyhow::bail!("Invalid snapshot request"),
            };
            let admission = PullAdmission {
                directory: self.directory.clone(),
                peer,
                recipient: self
                    .home
                    .cell()
                    .ok_or_else(|| anyhow::anyhow!("No native home"))?,
            };
            if let Request::PullSnapshot(request) = &request {
                codeyard_database::cells::pulls::Admission::check(
                    &admission,
                    request.binding.base(),
                    request.binding.head(),
                    None,
                )
                .await?;
            }
            let (mut writer, mut reader) = tokio::io::duplex(32768);
            let send = async {
                let mut received = 0_u64;
                let mut chunk = vec![0; 32768];
                loop {
                    let count = reader.read(&mut chunk).await?;
                    if count == 0 {
                        break;
                    }
                    received = received
                        .checked_add(count as u64)
                        .ok_or_else(|| anyhow::anyhow!("Fork snapshot exceeds its limit"))?;
                    ensure!(received <= maximum, "Fork snapshot exceeds its limit");
                    write_frame(output, &chunk[..count], 32768).await?;
                }
                anyhow::Ok(())
            };
            // Returning from generation closes the pipe even on an authorization failure.
            let generate = async {
                let result = match &request {
                    Request::Snapshot(request) => self
                        .home
                        .snapshot(request, &mut writer)
                        .await
                        .map(Reply::Snapshot),
                    Request::PullSnapshot(request) => self
                        .home
                        .pull_snapshot(request, &mut writer)
                        .await
                        .map(Reply::PullSnapshot),
                    _ => unreachable!(),
                };
                drop(writer);
                result
            };
            let (snapshot, ()) = tokio::try_join!(generate, send)?;
            match (&request, &snapshot) {
                (Request::Snapshot(request), Reply::Snapshot(snapshot)) => {
                    snapshot.validate(request)?
                }
                (Request::PullSnapshot(request), Reply::PullSnapshot(snapshot)) => {
                    if let Some(snapshot) = snapshot {
                        request.validate_snapshot(snapshot)?;
                    }
                    codeyard_database::cells::pulls::Admission::check(
                        &admission,
                        request.binding.base(),
                        request.binding.head(),
                        None,
                    )
                    .await?;
                }
                _ => anyhow::bail!("Invalid snapshot answer"),
            }
            self.admitted(peer).await?;
            write_frame(output, b"", 32768).await?;
            write_frame(output, &serde_json::to_vec(&snapshot)?, 128 * 1024).await?;
            Ok(())
        }))
    }
    fn admit<'a>(&'a self, peer: PublicKey) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.admitted(peer).await })
    }
    fn handle<'a>(&'a self, peer: PublicKey, bytes: &'a [u8]) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(codeyard_database::cells::authorization::scope(async move {
            self.admitted(peer).await?;
            let request: Request = serde_json::from_slice(bytes)?;
            let reply = match request {
                Request::Snapshot(_) | Request::PullSnapshot(_) => {
                    anyhow::bail!("Snapshot requires streaming")
                }
                Request::Push(event) => {
                    let admission = PullAdmission {
                        directory: self.directory.clone(),
                        peer,
                        recipient: self
                            .home
                            .cell()
                            .ok_or_else(|| anyhow::anyhow!("No native home"))?,
                    };
                    match self.home.receive_push(&event, &admission).await {
                        Ok(()) => Reply::Done,
                        Err(_) => Reply::Unavailable,
                    }
                }
                Request::Subscribe { binding, proof } => {
                    let admission = PullAdmission {
                        directory: self.directory.clone(),
                        peer,
                        recipient: self
                            .home
                            .cell()
                            .ok_or_else(|| anyhow::anyhow!("No native home"))?,
                    };
                    match self.home.receive_pull(&binding, &proof, &admission).await {
                        Ok(()) => Reply::Done,
                        Err(_) => Reply::Unavailable,
                    }
                }
                Request::PullGrant { binding, proof } => {
                    binding.validate()?;
                    if let Some(proof) = &proof {
                        proof.validate()?;
                    }
                    let admission = PullAdmission {
                        directory: self.directory.clone(),
                        peer,
                        recipient: self
                            .home
                            .cell()
                            .ok_or_else(|| anyhow::anyhow!("No native home"))?,
                    };
                    codeyard_database::cells::pulls::Admission::check(
                        &admission,
                        binding.head(),
                        binding.base(),
                        None,
                    )
                    .await?;
                    let result = self.home.pull_grant(&binding, proof.as_ref()).await;
                    codeyard_database::cells::pulls::Admission::check(
                        &admission,
                        binding.head(),
                        binding.base(),
                        None,
                    )
                    .await?;
                    match result {
                        Ok(grant) => Reply::PullGrant(grant),
                        Err(_) => Reply::Unavailable,
                    }
                }
                Request::RepositorySource { owner, name, proof } => {
                    match self.home.repository_source(&owner, &name, &proof).await {
                        Ok(source) => Reply::RepositorySource(source.map(Box::new)),
                        Err(_) => Reply::Unavailable,
                    }
                }
                Request::RepositoryId { id, proof } => {
                    match self.home.repository_id(id, &proof).await {
                        Ok(source) => Reply::RepositorySource(source.map(Box::new)),
                        Err(_) => Reply::Unavailable,
                    }
                }
                Request::RepositoryFork { owner, base, proof } => {
                    match self.home.repository_fork(&owner, base, &proof).await {
                        Ok(source) => Reply::RepositorySource(source.map(Box::new)),
                        Err(_) => Reply::Unavailable,
                    }
                }
                Request::Pointer(event) => {
                    let admission = PointerAdmission {
                        directory: self.directory.clone(),
                        peer,
                        recipient: self
                            .home
                            .cell()
                            .ok_or_else(|| anyhow::anyhow!("No native home"))?,
                    };
                    match self.home.receive_pointer(&event, &admission).await {
                        Ok(()) => Reply::Done,
                        Err(_) => Reply::Unavailable,
                    }
                }
                Request::NoticePublic(repository) => {
                    match self.home.notice_public(repository).await {
                        Ok(public) => Reply::NoticePublic(public),
                        Err(_) => Reply::Unavailable,
                    }
                }
                Request::Notice(event) => {
                    let admission = NoticeAdmission {
                        directory: self.directory.clone(),
                        peer,
                        recipient: self
                            .home
                            .cell()
                            .ok_or_else(|| anyhow::anyhow!("No native home"))?,
                    };
                    match self.home.receive_notice(&event, &admission).await {
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
                    codeyard_types::cell_ids::Prefix::for_id(id)?;
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
        }))
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
