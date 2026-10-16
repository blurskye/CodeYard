use super::wire::*;
use super::RpcHandler;
use crate::backend::{
    cluster::{link::Link, transport, NodeAddress, NodeContext},
    db,
    repositories::store::{ErrorKind, RepoError},
};
use anyhow::Result;
use uuid::Uuid;

impl NodeContext {
    pub async fn call(
        &self,
        node_id: i64,
        request: Request,
        body: &[u8],
    ) -> Result<(Reply, Vec<u8>), RepoError> {
        self.call_with_response_limit(node_id, request, body, MAX_BYTES)
            .await
    }

    pub(crate) async fn call_with_response_limit(
        &self,
        node_id: i64,
        request: Request,
        body: &[u8],
        response_body_limit: usize,
    ) -> Result<(Reply, Vec<u8>), RepoError> {
        if body.len() > operation_body_limit(&request.operation) {
            return Err(invalid("RPC frame exceeds its limit"));
        }
        let header = serde_json::to_vec(&request).map_err(|_| invalid("invalid RPC request"))?;
        if header.len() > MAX_JSON {
            return Err(invalid("RPC frame exceeds its limit"));
        }
        let first = self
            .exchange(node_id, &header, body, response_body_limit)
            .await;
        if first.is_ok() || !request.operation.is_read_only() {
            return first;
        }
        for (copy, moved) in self.read_copies(request.repository, node_id).await {
            let answer = self
                .exchange(copy, &header, body, response_body_limit)
                .await;
            if !cannot_serve(answer.as_ref().ok().map(|(reply, _)| reply), moved) {
                log_failover(request.repository, node_id, copy);
                return answer;
            }
        }
        first
    }

    /// One request and its reply from `node_id`. An `Err` means the node gave no reply.
    async fn exchange(
        &self,
        node_id: i64,
        header: &[u8],
        body: &[u8],
        response_body_limit: usize,
    ) -> Result<(Reply, Vec<u8>), RepoError> {
        if node_id == self.node_id {
            // Boxed: the handler's store operations lead back to `exchange`.
            return Box::pin(self.exchange_here(header, body, response_body_limit)).await;
        }
        let mut link = self.dial(node_id, transport::ALPN_RPC).await?;
        let result = tokio::time::timeout(TIMEOUT, async {
            write_frame(&mut link.writer, header, MAX_JSON).await?;
            write_frame(&mut link.writer, body, MAX_BYTES).await?;
            link.finish().await?;
            let reply = serde_json::from_slice(&read_frame(&mut link.reader, MAX_JSON).await?)?;
            let body = read_frame(&mut link.reader, response_body_limit).await?;
            Ok::<_, anyhow::Error>((reply, body))
        })
        .await;
        link.close(0, b"RPC complete");
        result
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())
    }

    /// The answer of this node's own handler. A node cannot dial itself, so a read that
    /// falls back to a copy held here is served in process, with the same checks.
    async fn exchange_here(
        &self,
        header: &[u8],
        body: &[u8],
        response_body_limit: usize,
    ) -> Result<(Reply, Vec<u8>), RepoError> {
        let request: Request = serde_json::from_slice(header).map_err(|_| unavailable())?;
        let handler = RpcHandler::new(self.clone());
        let (result, body) = match handler
            .execute(request, body.to_vec(), self.key.public_key())
            .await
        {
            Ok((value, body)) => (Ok(value), body),
            Err(failure) => (Err(failure), Vec::new()),
        };
        if body.len() > response_body_limit {
            return Err(unavailable());
        }
        Ok((Reply { result }, body))
    }

    /// Opens a request stream to `node_id` for `alpn`. A node whose last dial failed is
    /// refused at once for a few seconds.
    pub(super) async fn dial(&self, node_id: i64, alpn: &[u8]) -> Result<Link, RepoError> {
        if self.recently_unreachable(node_id) {
            return Err(unavailable());
        }
        let (node, address) = if let Some(pool) = &self.pool {
            let node = db::cluster::get_node_by_id(pool, node_id)
                .await
                .map_err(|_| unavailable())?
                .filter(|node| {
                    node.status == "active" && node.roles.iter().any(|role| role == "storage")
                })
                .ok_or_else(unavailable)?;
            let address: NodeAddress =
                serde_json::from_str(node.addr_hint.as_deref().ok_or_else(unavailable)?)
                    .map_err(|_| unavailable())?;
            let peer = crate::backend::cluster::parse_public_key(&node.public_key)
                .map_err(|_| unavailable())?;
            (peer, address)
        } else {
            let peer = self
                .peers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&node_id)
                .cloned()
                .ok_or_else(unavailable)?;
            (peer.node().map_err(|_| unavailable())?, peer.address)
        };
        let linked = transport::dial_node(&self.iroh, node, &address, alpn).await;
        self.note_dial(node_id, linked.is_ok());
        linked.map_err(|_| unavailable())
    }

    /// The nodes that may answer a read after `primary` gave no reply, best first: the
    /// repository's primary if it moved while the request waited, then online storage nodes
    /// other than `primary` whose copy has been brought up to date at least once, the most
    /// recently updated first. Only looked up after such a failure. Each node is paired with
    /// whether it is the primary that moved.
    pub(super) async fn read_copies(&self, repository: Uuid, primary: i64) -> Vec<(i64, bool)> {
        let Ok(pool) = self.database() else {
            return Vec::new();
        };
        let Ok(Some(row)) =
            db::cluster::get_repository_by_uuid(pool, &repository.to_string()).await
        else {
            return Vec::new();
        };
        let (Ok(replicas), Ok(nodes)) = (
            db::cluster::list_replicas(pool, row.id).await,
            db::cluster::list_nodes(pool).await,
        ) else {
            return Vec::new();
        };
        let mut copies: Vec<(i64, i64)> = replicas
            .into_iter()
            .filter(|&(node, applied)| {
                node != primary
                    && applied >= 0
                    && nodes.iter().any(|candidate| {
                        candidate.id == node
                            && candidate.is_online()
                            && candidate.roles.iter().any(|role| role == "storage")
                    })
            })
            .collect();
        copies.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let moved = row.storage_node_id.filter(|node| *node != primary);
        moved
            .map(|node| (node, true))
            .into_iter()
            .chain(copies.into_iter().map(|(node, _)| (node, false)))
            .collect()
    }
}

/// Whether a candidate's reply means it cannot serve the read: no reply at all, or a reply
/// that says so. A replica that reports any unavailable error gives the read to the next
/// copy. The primary that moved answers for its repository, so for it only a reply saying
/// the node is not serving does; its other errors are the answer to the read.
pub(super) fn cannot_serve(reply: Option<&Reply>, moved_primary: bool) -> bool {
    match reply {
        None => true,
        Some(reply) => match &reply.result {
            Err(Failure::Repository(error)) if moved_primary => error.is_storage_node_unavailable(),
            Err(Failure::Repository(error)) => error.kind == ErrorKind::Unavailable,
            _ => false,
        },
    }
}

/// One warning for each read that a copy answered because the primary did not reply.
pub(super) fn log_failover(repository: Uuid, primary: i64, copy: i64) {
    tracing::warn!(
        repository = %repository,
        primary,
        copy,
        "primary unreachable; read served by a copy"
    );
}
