//! Repository protocol composition; wire format and policy have explicit owners.
use anyhow::Result;
use codeyard_cluster_core::link::read_frame;
use codeyard_cluster_core::link::write_frame;
use codeyard_cluster_core::link::Link;
use codeyard_db_core::limits::MAX_JSON;
use codeyard_repos_core::context::NodeContext;
use codeyard_repos_core::wire::invalid;
use codeyard_repos_core::wire::unavailable;
use codeyard_repos_core::wire::Operation;
use codeyard_repos_core::wire::Reply;
use codeyard_repos_core::wire::Request;
use codeyard_repos_core::wire::MAX_BYTES;
use codeyard_repos_core::wire::TIMEOUT;
use iroh::endpoint::Connection;
use iroh::protocol::AcceptError;
use iroh::protocol::ProtocolHandler;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::Semaphore;
use tokio_util::task::task_tracker::TrackedFuture;
use tokio_util::task::TaskTracker;
mod git_session;
mod objects;
pub mod repository;

/// Answers a request with a refusal and ends the stream. A peer that does not read cannot
/// hold the caller's slot past the timeout.
async fn refuse(link: &mut Link, error: codeyard_git::error::RepoError) -> Result<()> {
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

/// Repository requests this node is serving, buffered or streamed. Shutdown closes admission
/// and waits for requests already running under the caller's shared drain deadline.
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

    /// Gives already-admitted work an independent lifetime. Dropping the reply waiter
    /// must not cancel committed claims or post-publication bookkeeping.
    pub fn spawn_admitted<F>(&self, work: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        tokio::spawn(self.running.track_future(work))
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
    object_slots: Arc<Semaphore>,
    session_headers: Arc<Semaphore>,
    headers: Arc<Mutex<HashMap<iroh::PublicKey, Arc<Semaphore>>>>,
    sessions: SessionGate,
}

impl RpcHandler {
    pub fn new(context: NodeContext) -> Self {
        Self {
            context,
            slots: Arc::new(Semaphore::new(16)),
            object_slots: Arc::new(Semaphore::new(2)),
            session_headers: Arc::new(Semaphore::new(16)),
            headers: Default::default(),
            sessions: SessionGate::default(),
        }
    }

    /// The gate this handler admits all repository requests through.
    pub fn sessions(&self) -> SessionGate {
        self.sessions.clone()
    }

    fn header_slot(&self, peer: iroh::PublicKey) -> Result<tokio::sync::OwnedSemaphorePermit> {
        codeyard_cluster_core::transport::peer_header_slot(&self.headers, peer)
    }

    /// Serves one request stream. An error ends the connection the stream came on.
    pub async fn serve_link(&self, link: Link) -> Result<()> {
        let Some(request) = self.sessions.admit(self.answer_link(link)) else {
            return Ok(());
        };
        request.await
    }

    async fn answer_link(&self, mut link: Link) -> Result<()> {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let slot = self.slots.clone().try_acquire_owned()?;
        let peer = link.peer;
        let header_slot = self.header_slot(peer)?;
        // Membership is checked before reading headers, also on reused connections.
        // A bad request gets an error reply; an oversized or broken frame ends the connection.
        let request = tokio::time::timeout_at(deadline, async {
            if !codeyard_cluster_core::transport::active_member(self.context.database()?, peer)
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
            Operation::GitSession(_)
                | Operation::ForkPack { .. }
                | Operation::ReplicaPack { .. }
                | Operation::WritePack { .. }
        ) {
            let session_header = self.session_headers.clone().try_acquire_owned()?;
            drop(slot);
            // A session that arrives after shutdown began is refused. Its connection closes
            // unanswered, which the peer reports as unavailable.
            let Some(session) = self.sessions.admit(self.serve_session(
                request,
                link,
                deadline,
                (session_header, header_slot),
            )) else {
                return Ok(());
            };
            return session.await;
        }
        drop(header_slot);
        let _object_slot = if request.operation.is_object() {
            Some(self.object_slots.clone().try_acquire_owned()?)
        } else {
            None
        };
        let body_limit = codeyard_repos_core::wire::operation_body_limit(&request.operation);
        tokio::time::timeout_at(deadline, async {
            if body_limit > 0 {
                if let Err(error) = self.authorize_body_request(&request, peer).await {
                    refuse(&mut link, error).await?;
                    return Ok(());
                }
            }
            let body = read_frame(&mut link.reader, body_limit).await?;
            // This second pass is intentionally live: authority can change while an
            // allowed editor upload is still arriving.
            let result = if request.operation.is_read_only() {
                self.execute(request, body, peer).await
            } else {
                let handler = self.clone();
                self.sessions
                    .spawn_admitted(async move {
                        // Retain admission until the mutation really ends, not until its
                        // response deadline or connection expires.
                        let _slot = slot;
                        let result = handler.execute(request, body, peer).await;
                        if let Err(error) = &result {
                            tracing::warn!(?error, "admitted repository mutation failed");
                        }
                        result
                    })
                    .await?
            };
            let (reply, body) = match result {
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
        let Ok(_connection) = codeyard_cluster_core::transport::connection_slot(&connection) else {
            return Ok(());
        };
        // Each stream the peer opens is one request, answered in turn.
        while let Some(link) = codeyard_cluster_core::transport::accept_stream(&connection).await {
            if let Err(error) = self.serve_link(link).await {
                tracing::warn!(%error, "repository RPC failed");
                connection.close(500u32.into(), b"RPC request failed");
                break;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct RpcFactory;
impl codeyard_repos_core::local_rpc::RpcFactory for RpcFactory {
    fn create(&self, context: NodeContext) -> Box<dyn codeyard_repos_core::local_rpc::LocalRpc> {
        Box::new(RpcHandler::new(context))
    }
}
impl codeyard_repos_core::local_rpc::LocalRpc for RpcHandler {
    fn execute(
        &self,
        request: Request,
        body: Vec<u8>,
        caller: iroh::PublicKey,
    ) -> futures_util::future::BoxFuture<
        '_,
        Result<(serde_json::Value, Vec<u8>), codeyard_repos_core::wire::Failure>,
    > {
        Box::pin(self.execute(request, body, caller))
    }
    fn own_copy<'a>(
        &'a self,
        request: &'a Request,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<codeyard_repos_core::local_rpc::LocalCopy, codeyard_git::error::RepoError>,
    > {
        Box::pin(self.own_copy(request))
    }
}

#[cfg(test)]
mod regression_tests;

#[cfg(test)]
mod tests {
    use crate::SessionGate;
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
