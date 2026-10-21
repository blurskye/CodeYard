//! CI queue RPCs. The authenticated node key owns every worker id it supplies.
use super::{link::Link, rpc, transport, NodeContext};
use crate::backend::{
    ci::queue::{Operation, Reply, Request, RunQueue, CORE_UNAVAILABLE},
    db,
};
use anyhow::{ensure, Result};
use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::Semaphore;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        ci::{CiConfig, CiExecutionMode},
        cluster::{IrohDialer, NodeKey, Relay},
    };

    #[tokio::test]
    async fn draining_refuses_claims_but_still_accepts_heartbeats_and_finishes() {
        let root = tempfile::tempdir().unwrap();
        let pool = db::init_db(&format!(
            "sqlite://{}",
            root.path().join("test.db").display()
        ))
        .await
        .unwrap();
        db::run_migrations(&pool).await.unwrap();
        let key = NodeKey::generate();
        let node = db::cluster::register_own_node(
            &pool,
            &key.public_hex(),
            "core",
            &["actions-worker".into()],
            "test",
        )
        .await
        .unwrap();
        let context = NodeContext {
            pool: Some(pool.clone()),
            iroh: IrohDialer::new(Relay::Off, key.clone()),
            key,
            node_id: node.id,
            repos_path: root.path().to_owned(),
            peers: Default::default(),
            unreachable: Default::default(),
            ci_replicas: Default::default(),
        };
        let queue = RunQueue::Database {
            pool: pool.clone(),
            config: Arc::new(CiConfig::default()),
            secrets: None,
            context: Some(context.clone()),
            notifications: Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                &pool,
            )),
        };
        let handler = CiHandler::new(context.clone(), queue);
        handler.stop_claiming();
        let request = |operation| Request {
            worker: "worker/1".into(),
            operation,
        };
        let error = handler
            .execute(
                context.key.public_key(),
                request(Operation::Claim {
                    mode: CiExecutionMode::Local,
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), CORE_UNAVAILABLE);
        let header = serde_json::to_vec(&request(Operation::Claim {
            mode: CiExecutionMode::Local,
        }))
        .unwrap();
        let mut frame = Vec::new();
        frame.extend_from_slice(&(header.len() as u32).to_be_bytes());
        frame.extend_from_slice(&header);
        let mut link = Link::tcp(
            context.key.public_key(),
            transport::ALPN_EVENTS,
            std::io::Cursor::new(frame),
            tokio::io::sink(),
        );
        assert_eq!(
            handler.answer(&mut link).await.unwrap(),
            Err(CORE_UNAVAILABLE.into())
        );
        assert!(handler
            .execute(
                context.key.public_key(),
                request(Operation::Heartbeat { id: 999 })
            )
            .await
            .is_ok());
        assert!(handler
            .execute(
                context.key.public_key(),
                request(Operation::Finish {
                    id: 999,
                    status: "success".into(),
                    message: None
                })
            )
            .await
            .is_ok());
        pool.close().await;
    }
}

const REFUSED: &str = "CI queue request refused";

#[derive(Clone)]
pub struct CiHandler {
    context: NodeContext,
    queue: RunQueue,
    slots: Arc<Semaphore>,
    accepting_claims: Arc<AtomicBool>,
}

impl std::fmt::Debug for CiHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CiHandler").finish_non_exhaustive()
    }
}

impl CiHandler {
    pub fn new(context: NodeContext, queue: RunQueue) -> Self {
        Self {
            context,
            queue,
            slots: Arc::new(Semaphore::new(16)),
            accepting_claims: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(super) fn stop_claiming(&self) {
        self.accepting_claims.store(false, Ordering::Release);
    }

    async fn execute(&self, peer: iroh::PublicKey, request: Request) -> Result<serde_json::Value> {
        ensure!(
            !request.worker.is_empty()
                && request.worker.len() <= 128
                && request
                    .worker
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)),
            "invalid CI worker id"
        );
        let worker = format!("{peer}/{}", request.worker);
        let value = match request.operation {
            Operation::Claim { mode } => {
                ensure!(
                    self.accepting_claims.load(Ordering::Acquire),
                    CORE_UNAVAILABLE
                );
                let mut claim = self.queue.claim(&worker, mode).await?;
                if let Some(claim) = &mut claim {
                    claim.storage_node_id = claim.storage_node_id.or(Some(self.context.node_id));
                }
                serde_json::to_value(claim)?
            }
            Operation::Storage { run_id } => {
                let pool = self.context.database()?;
                let run = db::ci::get_ci_run_by_run_id(pool, run_id)
                    .await?
                    .filter(|run| run.status == "running" && run.worker.as_deref() == Some(&worker))
                    .ok_or_else(|| anyhow::anyhow!("CI run does not belong to this worker"))?;
                let repo = db::repositories::records::get_repository_by_id(pool, run.repo_id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("repository not found"))?;
                let id = repo.storage_node_id.unwrap_or(self.context.node_id);
                let node = db::cluster::get_node_by_id(pool, id)
                    .await?
                    .filter(|node| {
                        node.status == "active" && node.roles.iter().any(|role| role == "storage")
                    })
                    .ok_or_else(|| anyhow::anyhow!(CORE_UNAVAILABLE))?;
                let peer = super::CorePeer {
                    public_key: node.public_key,
                    address: serde_json::from_str(
                        node.addr_hint
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!(CORE_UNAVAILABLE))?,
                    )?,
                };
                serde_json::to_value((id, peer))?
            }
            Operation::Heartbeat { id } => {
                serde_json::to_value(self.queue.heartbeat(&worker, id).await?)?
            }
            Operation::Logs { id, lines } => {
                serde_json::to_value(self.queue.logs(&worker, id, &lines).await?)?
            }
            Operation::Finish {
                id,
                status,
                message,
            } => serde_json::to_value(
                self.queue
                    .finish(&worker, id, &status, message.as_deref())
                    .await?,
            )?,
        };
        Ok(value)
    }

    /// Why this node refuses a CI request from `peer` now, or `None` when it may answer. This
    /// node must be an active member with its own key, and the caller an active actions-worker.
    /// A check that cannot read the database refuses too.
    async fn refusal(&self, peer: iroh::PublicKey) -> Option<&'static str> {
        let checked = async {
            let pool = self.context.database()?;
            let serving = db::cluster::get_node_by_id(pool, self.context.node_id)
                .await?
                .is_some_and(|node| {
                    node.status == "active" && node.public_key == self.context.key.public_hex()
                });
            let worker = db::cluster::get_node_by_public_key(pool, &peer.to_string())
                .await?
                .is_some_and(|node| {
                    node.status == "active"
                        && node.roles.iter().any(|role| role == "actions-worker")
                });
            Ok::<_, anyhow::Error>((serving, worker))
        };
        match checked.await {
            Ok((true, true)) => None,
            Ok((true, false)) => Some("not an active actions-worker"),
            Ok((false, _)) => Some(CORE_UNAVAILABLE),
            Err(error) => {
                tracing::warn!(%error, "CI queue membership check failed");
                Some(CORE_UNAVAILABLE)
            }
        }
    }

    /// The reply to the request on `link`. An error means the frame is broken, and it ends
    /// the connection instead.
    async fn answer(&self, link: &mut Link) -> Result<Result<serde_json::Value, String>> {
        let peer = link.peer;
        if let Some(refusal) = self.refusal(peer).await {
            return Ok(Err(refusal.to_owned()));
        }
        tracing::trace!(name: "ci_request_authorized", %peer, "CI queue request authorized; waiting for frame");
        let header = rpc::read_frame(&mut link.reader, rpc::MAX_JSON).await?;
        let request = match serde_json::from_slice::<Request>(&header) {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%error, "CI queue request is invalid");
                return Ok(Err(REFUSED.to_owned()));
            }
        };
        // The frame can wait on the network while membership changes, so check again
        // before anything runs.
        if let Some(refusal) = self.refusal(peer).await {
            return Ok(Err(refusal.to_owned()));
        }
        if let Ok(pool) = self.context.database() {
            if let Err(error) = db::cluster::touch_node(pool, &peer.to_string()).await {
                tracing::debug!(%error, "could not record a worker's last request");
            }
        }
        Ok(match self.execute(peer, request).await {
            Ok(value) => Ok(value),
            Err(error) => {
                // Never return database errors or decrypted secret contents to the peer.
                tracing::warn!(%error, "CI queue request failed");
                Err(if error.to_string() == CORE_UNAVAILABLE {
                    CORE_UNAVAILABLE.to_owned()
                } else {
                    REFUSED.to_owned()
                })
            }
        })
    }

    /// Answers the one request on `link`.
    pub(crate) async fn serve_link(&self, mut link: Link) -> Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            // A busy core refuses at once, as pairing does, so the client asks another
            // core instead of waiting in a queue. Hold admission through the reply write.
            let slot = self.slots.try_acquire();
            let result = match &slot {
                Ok(_) => self.answer(&mut link).await?,
                Err(_) => Err(CORE_UNAVAILABLE.to_owned()),
            };
            let header = serde_json::to_vec(&Reply { result })?;
            rpc::write_frame(&mut link.writer, &header, rpc::MAX_JSON).await?;
            link.finish().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }
}

impl ProtocolHandler for CiHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        while let Some(link) = transport::accept_stream(&connection).await {
            if let Err(error) = self.serve_link(link).await {
                tracing::warn!(%error, "CI queue RPC failed");
                connection.close(500u32.into(), b"CI request failed");
                break;
            }
        }
        Ok(())
    }
}
