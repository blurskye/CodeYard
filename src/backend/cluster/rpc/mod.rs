//! Repository protocol composition; wire format and policy have explicit owners.
mod client;
mod git_session;
mod repository;
pub mod wire;
use crate::backend::cluster::{link::Link, transport, NodeContext};
use anyhow::Result;
use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
pub use repository::{authorize_operation, repository_uuid};
use std::sync::Arc;
use tokio::sync::Semaphore;
use wire::invalid;
pub use wire::{
    read_frame, unavailable, write_frame, CiRunAccess, Failure, Operation, Reply, Request,
    MAX_BYTES, MAX_JSON, TIMEOUT,
};

/// Answers a request with a refusal and ends the stream. A peer that does not read cannot
/// hold the caller's slot past the timeout.
async fn refuse(
    link: &mut Link,
    error: crate::backend::repositories::store::RepoError,
) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        write_frame(
            &mut link.writer,
            &serde_json::to_vec(&Reply {
                result: Err(error.into()),
            })?,
            MAX_JSON,
        )
        .await?;
        write_frame(&mut link.writer, b"", MAX_BYTES).await?;
        link.finish().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[derive(Debug, Clone)]
pub struct RpcHandler {
    context: NodeContext,
    slots: Arc<Semaphore>,
}

impl RpcHandler {
    pub fn new(context: NodeContext) -> Self {
        Self {
            context,
            slots: Arc::new(Semaphore::new(16)),
        }
    }

    /// Serves one request stream. An error ends the connection the stream came on.
    pub(crate) async fn serve_link(&self, mut link: Link) -> Result<()> {
        let slot = tokio::time::timeout(TIMEOUT, self.slots.acquire()).await??;
        let peer = link.peer;
        // Membership is checked before reading headers, also on reused connections.
        // A bad request gets an error reply; an oversized or broken frame ends the connection.
        let request = tokio::time::timeout(TIMEOUT, async {
            if !transport::active_member(self.context.database()?, peer)
                .await
                .map_err(|_| unavailable())?
            {
                return Ok(Err(invalid("not an active cluster member")));
            }
            let header = read_frame(&mut link.reader, MAX_JSON).await?;
            Ok::<_, anyhow::Error>(
                serde_json::from_slice::<Request>(&header)
                    .map_err(|_| invalid("invalid RPC request")),
            )
        })
        .await??;
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                refuse(&mut link, error).await?;
                return Ok(());
            }
        };
        if matches!(
            request.operation,
            Operation::GitSession(_) | Operation::ReplicaPack { .. }
        ) {
            drop(slot);
            return self.serve_session(request, link).await;
        }
        let body_limit = wire::operation_body_limit(&request.operation);
        if body_limit > 0 {
            if let Err(error) = self.authorize_body_request(&request, peer).await {
                refuse(&mut link, error).await?;
                return Ok(());
            }
        }
        tokio::time::timeout(TIMEOUT, async {
            let body = read_frame(&mut link.reader, body_limit).await?;
            // This second pass is intentionally live: authority can change while an
            // allowed editor upload is still arriving.
            let (reply, body) = match self.execute(request, body, peer).await {
                Ok((value, body)) => (Reply { result: Ok(value) }, body),
                Err(error) => (Reply { result: Err(error) }, Vec::new()),
            };
            let mut header = serde_json::to_vec(&reply)?;
            if header.len() > MAX_JSON {
                header = serde_json::to_vec(&Reply {
                    result: Err(invalid("RPC frame exceeds its limit").into()),
                })?;
            }
            write_frame(&mut link.writer, &header, MAX_JSON).await?;
            write_frame(&mut link.writer, &body, MAX_BYTES).await?;
            link.finish().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }
}

impl ProtocolHandler for RpcHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // Each stream the peer opens is one request, answered in turn.
        while let Some(link) = transport::accept_stream(&connection).await {
            if let Err(error) = self.serve_link(link).await {
                tracing::debug!(%error, "repository RPC failed");
                connection.close(413u32.into(), b"invalid or oversized RPC frame");
                break;
            }
        }
        Ok(())
    }
}
