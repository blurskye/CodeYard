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
use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::sync::Semaphore;
use tokio_util::task::{task_tracker::TrackedFuture, TaskTracker};
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

/// The repository Git sessions this node is serving. Shutdown closes the gate, so no session
/// starts after it, and then waits for the sessions already running. The caller bounds that
/// wait; a session still running when the bound passes ends with its connection.
#[derive(Clone, Debug)]
pub struct SessionGate {
    open: Arc<Mutex<bool>>,
    running: TaskTracker,
}

impl Default for SessionGate {
    fn default() -> Self {
        Self {
            open: Arc::new(Mutex::new(true)),
            running: TaskTracker::new(),
        }
    }
}

impl SessionGate {
    /// Tracks `session` while it runs. Once the gate is closed, the session is dropped instead,
    /// which closes its connection.
    pub fn admit<F: Future>(&self, session: F) -> Option<TrackedFuture<F>> {
        let open = self.open.lock().unwrap_or_else(|error| error.into_inner());
        (*open).then(|| self.running.track_future(session))
    }

    /// Refuses sessions from now on. Sessions already admitted keep running.
    pub fn close(&self) {
        *self.open.lock().unwrap_or_else(|error| error.into_inner()) = false;
        self.running.close();
    }

    /// Closes the gate, then waits until every admitted session has finished.
    pub async fn drain(&self) {
        self.close();
        self.running.wait().await;
    }
}

#[derive(Debug, Clone)]
pub struct RpcHandler {
    context: NodeContext,
    slots: Arc<Semaphore>,
    sessions: SessionGate,
}

impl RpcHandler {
    pub fn new(context: NodeContext) -> Self {
        Self {
            context,
            slots: Arc::new(Semaphore::new(16)),
            sessions: SessionGate::default(),
        }
    }

    /// The gate this handler admits its Git sessions through.
    pub fn sessions(&self) -> SessionGate {
        self.sessions.clone()
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
            // A session that arrives after shutdown began is refused. Its connection closes
            // unanswered, which the peer reports as unavailable.
            let Some(session) = self.sessions.admit(self.serve_session(request, link)) else {
                return Ok(());
            };
            return session.await;
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

#[cfg(test)]
mod tests {
    use super::SessionGate;
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    };

    #[tokio::test]
    async fn drain_waits_for_an_admitted_session_and_admits_nothing_after() {
        let gate = SessionGate::default();
        let finished = Arc::new(AtomicBool::new(false));
        let flag = finished.clone();
        let session = gate
            .admit(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                flag.store(true, Ordering::SeqCst);
            })
            .expect("an open gate admits a session");
        let session = tokio::spawn(session);
        tokio::time::timeout(Duration::from_secs(5), gate.drain())
            .await
            .expect("drain returns once the session has finished");
        assert!(
            finished.load(Ordering::SeqCst),
            "drain returned before the session finished"
        );
        session.await.unwrap();
        assert!(
            gate.admit(async {}).is_none(),
            "a drained gate refuses new sessions"
        );
    }

    #[tokio::test]
    async fn a_session_past_the_deadline_is_released_when_its_connection_ends() {
        let gate = SessionGate::default();
        let session = gate
            .admit(std::future::pending::<()>())
            .expect("an open gate admits a session");
        gate.close();
        // The shutdown deadline passes while the session still runs.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), gate.drain())
                .await
                .is_err(),
            "drain must not finish while a session runs"
        );
        // Shutdown then ends the connection, which drops the session.
        drop(session);
        tokio::time::timeout(Duration::from_secs(5), gate.drain())
            .await
            .expect("dropping the session releases the wait");
    }
}
