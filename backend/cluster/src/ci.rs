//! CI queue RPCs. The authenticated node key owns every worker id it supplies.
use codeyard_repos_core::ci::queue::Operation;
use codeyard_repos_core::ci::queue::Reply;
use codeyard_repos_core::ci::queue::Request;
use codeyard_repos_core::ci::queue::RunQueue;
use codeyard_repos_core::ci::queue::CORE_UNAVAILABLE;
use codeyard_repos_core::context::NodeContext;
use anyhow::ensure;
use anyhow::Result;
use codeyard_cluster_core::link::Link;
use iroh::endpoint::Connection;
use iroh::protocol::AcceptError;
use iroh::protocol::ProtocolHandler;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::Semaphore;

fn transient_database_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| match cause.downcast_ref::<sqlx::Error>() {
            Some(
                sqlx::Error::PoolTimedOut
                | sqlx::Error::Io(_)
                | sqlx::Error::Tls(_)
                | sqlx::Error::WorkerCrashed,
            ) => true,
            Some(sqlx::Error::Database(error)) => error.code().is_some_and(|code| {
                matches!(
                    code.as_ref(),
                    "40001"
                        | "40P01"
                        | "55P03"
                        | "57014"
                        | "57P01"
                        | "57P02"
                        | "57P03"
                        | "53300"
                        | "5"
                        | "6"
                )
            }),
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeyard_ci::config::CiConfig;
    use codeyard_ci::config::CiExecutionMode;
    use codeyard_cluster_core::transport::IrohDialer;
    use codeyard_cluster_core::transport::Relay;
    use codeyard_support::node_identity::NodeKey;

    async fn test_context() -> (tempfile::TempDir, codeyard_db_core::DbPool, NodeContext) {
        let root = tempfile::tempdir().unwrap();
        let pool = codeyard_db_core::init_db(&format!(
            "sqlite://{}",
            root.path().join("test.db").display()
        ))
        .await
        .unwrap();
        codeyard_db_core::run_migrations(&pool).await.unwrap();
        let key = NodeKey::generate();
        let node = codeyard_db_cluster::cluster::register_own_node(
            &pool,
            &key.public_hex(),
            "core",
            // Claims need web or ssh, which may read repositories held by other nodes.
            &["web".into(), "runner".into()],
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
        (root, pool, context)
    }

    #[tokio::test]
    async fn draining_refuses_claims_but_still_accepts_heartbeats_and_finishes() {
        let (_root, pool, context) = test_context().await;
        let queue = RunQueue::Database {
            pool: pool.clone(),
            config: Arc::new(CiConfig::default()),
            secrets: None,
            context: Some(context.clone()),
            notifications: Arc::new(
                codeyard_notifications::broadcast::NotificationBroadcaster::new(&pool),
            ),
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
                    platforms: CiConfig::default().platform_labels(),
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), CORE_UNAVAILABLE);
        let header = serde_json::to_vec(&request(Operation::Claim {
            mode: CiExecutionMode::Local,
            platforms: CiConfig::default().platform_labels(),
        }))
        .unwrap();
        let mut frame = Vec::new();
        frame.extend_from_slice(&(header.len() as u32).to_be_bytes());
        frame.extend_from_slice(&header);
        let mut link = Link::tcp(
            context.key.public_key(),
            codeyard_cluster_core::transport::ALPN_EVENTS,
            std::io::Cursor::new(frame),
            tokio::io::sink(),
        );
        let slot = Arc::new(handler.slots.clone().try_acquire_owned().unwrap());
        assert_eq!(
            handler.answer(&mut link, slot).await.unwrap(),
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

    #[tokio::test]
    async fn oversized_environment_claim_fails_instead_of_leaving_an_undeliverable_lease() {
        use codeyard_db_actions::environment_values as values;
        use codeyard_db_actions::environment_values::Save;
        use codeyard_db_orgs::organization_values::Kind;
        use codeyard_notifications::broadcast::NotificationBroadcaster;
        use codeyard_support::secrets::SecretEncryptor;

        let (root, pool, context) = test_context().await;
        let owner = codeyard_db_identity::users::accounts::create_user(&pool, "owner", None)
            .await
            .unwrap();
        let repo = codeyard_repos_core::repositories::lifecycle::provision(
            (&pool, None),
            root.path(),
            codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner.id),
            "project",
            None,
            true,
            None,
        )
        .await
        .unwrap();
        let blob=git2::Repository::open_bare(&repo.disk_path).unwrap().blob(b"on: push\njobs:\n  deploy:\n    runs-on: linux\n    environment: production\n    steps: [{run: 'true'}]\n").unwrap().to_string();
        let encryptor = Arc::new(SecretEncryptor::new(&[0x63; 32]));
        values::create(&pool, owner.id, repo.id, "production")
            .await
            .unwrap();
        let value = "\u{1}".repeat(20 * 1024);
        // Both source sets are within their 1 MiB limits, but JSON escapes expand the reply.
        for index in 0..50 {
            let name = format!("SECRET_{index}");
            codeyard_db_actions::secrets::create_or_update_secret(
                &pool,
                &encryptor,
                repo.id,
                &name,
                &value,
                Some(owner.id),
                &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&{
                    let session = uuid::Uuid::new_v4().simple().to_string().repeat(2);
                    codeyard_db_identity::users::sessions::create_session(
                        &pool,
                        &session,
                        owner.id,
                        None,
                        None,
                        chrono::Utc::now() + chrono::Duration::hours(1),
                    )
                    .await
                    .unwrap();
                    session
                }),
            )
            .await
            .unwrap();
            values::save(
                &pool,
                Some(&encryptor),
                owner.id,
                repo.id,
                "production",
                &Save {
                    kind: Kind::Secret,
                    previous: None,
                    name: &name,
                    value: Some(&value),
                },
            )
            .await
            .unwrap();
        }
        let run = codeyard_db_actions::ci::enqueue_ci_run(
            &pool,
            codeyard_db_actions::ci::NewCiRun {
                repo_id: repo.id,
                triggered_by_id: owner.id,
                pipeline: "ci",
                event: "push",
                pull_request_id: None,
                branch: "main",
                head_sha: &"1".repeat(40),
                workflow_path: ".github/workflows/ci.yml",
                workflow_blobs: &serde_json::json!({".github/workflows/ci.yml":blob}),
                workflow_inputs: None,
                environment: Some("production"),
                manual: false,
            },
            &[owner.id],
        )
        .await
        .unwrap();
        let queue = RunQueue::Database {
            pool: pool.clone(),
            config: Arc::new(CiConfig {
                mode: CiExecutionMode::Local,
                trusted_actor_ids: std::collections::BTreeSet::from([owner.id]),
                ..CiConfig::default()
            }),
            secrets: Some(encryptor),
            context: None,
            notifications: Arc::new(NotificationBroadcaster::new(&pool)),
        };
        let handler = CiHandler::new(context.clone(), queue);
        let error = handler
            .execute(
                context.key.public_key(),
                Request {
                    worker: "worker/1".into(),
                    operation: Operation::Claim {
                        mode: CiExecutionMode::Local,
                        platforms: CiConfig::default().platform_labels(),
                    },
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), CLAIM_TOO_LARGE);
        let current = codeyard_db_actions::ci::get_ci_run_by_run_id(&pool, run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.status, "failed");
        assert_eq!(current.error_message.as_deref(), Some(CLAIM_TOO_LARGE));
        assert!(handler
            .queue
            .claim(
                "another-worker",
                CiExecutionMode::Local,
                &CiConfig::default().platform_labels()
            )
            .await
            .unwrap()
            .is_none());
        pool.close().await;
    }
}

const REFUSED: &str = "CI queue request refused";
const CLAIM_TOO_LARGE: &str = "CI claim exceeds the RPC response limit";

#[derive(Clone)]
pub struct CiHandler {
    context: NodeContext,
    queue: RunQueue,
    slots: Arc<Semaphore>,
    accepting_claims: Arc<AtomicBool>,
    headers: Arc<std::sync::Mutex<std::collections::HashMap<iroh::PublicKey, Arc<Semaphore>>>>,
    sessions: codeyard_repos_core::cluster::rpc::SessionGate,
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
            headers: Default::default(),
            sessions: codeyard_repos_core::cluster::rpc::SessionGate::default(),
        }
    }

    /// Opens claim admission once the node is published. A claim assigns runs to repositories
    /// placed on this node, and this node's address must be recorded before it can hand them out.
    pub(super) fn start_claiming(&self) {
        self.accepting_claims.store(true, Ordering::Release);
    }

    pub(super) fn stop_claiming(&self) {
        self.accepting_claims.store(false, Ordering::Release);
    }

    pub fn close(&self) {
        self.stop_claiming();
        self.sessions.close();
    }

    pub async fn drain(&self) {
        self.sessions.drain().await;
    }

    /// Whether this core's node may read repositories held by other nodes. Only web and ssh
    /// nodes may, and claim preparation reads each run's workflow that way.
    async fn may_read_repositories(&self) -> Result<bool> {
        let node = codeyard_db_cluster::cluster::get_node_by_id(
            self.context.database()?,
            self.context.node_id,
        )
        .await?;
        Ok(node.is_some_and(|node| node.roles.iter().any(|role| role == "web" || role == "ssh")))
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
            Operation::Claim { mode, platforms } => {
                ensure!(
                    platforms.len() <= 1000
                        && platforms
                            .iter()
                            .all(|label| !label.is_empty() && label.len() <= 128),
                    "invalid runner platforms"
                );
                ensure!(
                    self.accepting_claims.load(Ordering::Acquire),
                    CORE_UNAVAILABLE
                );
                // Refuse before dequeuing: a run claimed here could not be prepared, and the
                // worker would then fail it instead of asking another core.
                ensure!(self.may_read_repositories().await?, CORE_UNAVAILABLE);
                let mut claim = self.queue.claim(&worker, mode, &platforms).await?;
                if let Some(refusal) = self.refusal(peer).await {
                    if let Some(claim) = &claim {
                        self.queue
                            .finish(
                                &worker,
                                claim.run.id,
                                "failed",
                                Some("Runner authority lost during preparation"),
                            )
                            .await?;
                    }
                    anyhow::bail!(refusal);
                }
                if let Some(claim) = &mut claim {
                    claim.storage_node_id = claim.storage_node_id.or(Some(self.context.node_id));
                }
                let value = serde_json::to_value(&claim)?;
                let bytes = serde_json::to_vec(&Reply {
                    result: Ok(value.clone()),
                })?;
                if bytes.len() > codeyard_db_core::limits::MAX_JSON {
                    if let Some(claim) = &claim {
                        self.queue
                            .finish(&worker, claim.run.id, "failed", Some(CLAIM_TOO_LARGE))
                            .await?;
                    }
                    anyhow::bail!(CLAIM_TOO_LARGE);
                }
                value
            }
            Operation::Storage { run_id } => {
                let pool = self.context.database()?;
                let run = codeyard_db_actions::ci::get_ci_run_by_run_id(pool, run_id)
                    .await?
                    .filter(|run| run.status == "running" && run.worker.as_deref() == Some(&worker))
                    .ok_or_else(|| anyhow::anyhow!("CI run does not belong to this worker"))?;
                let repo = codeyard_db_repos::repositories::records::get_repository_by_id(
                    pool,
                    run.repo_id,
                )
                .await?
                .ok_or_else(|| anyhow::anyhow!("repository not found"))?;
                let id = repo.storage_node_id.unwrap_or(self.context.node_id);
                let node = codeyard_db_cluster::cluster::get_node_by_id(pool, id)
                    .await?
                    .filter(|node| {
                        node.status == "active" && node.roles.iter().any(|role| role == "storage")
                    })
                    .ok_or_else(|| anyhow::anyhow!(CORE_UNAVAILABLE))?;
                let peer = codeyard_cluster_core::worker::CorePeer {
                    target: None,
                    public_key: node.public_key,
                    address: serde_json::from_str(
                        node.addr_hint
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!(CORE_UNAVAILABLE))?,
                    )?,
                };
                serde_json::to_value((id, peer))?
            }
            Operation::Progress {
                id,
                generation,
                workflow,
                event,
            } => serde_json::to_value(
                self.queue
                    .progress(&worker, id, &generation, &workflow, &event)
                    .await?,
            )?,
            Operation::JobState {
                id,
                generation,
                workflow,
                state,
            } => serde_json::to_value(
                self.queue
                    .job_state(&worker, id, &generation, &workflow, &state)
                    .await?,
            )?,
            Operation::AdmitJob {
                id,
                generation,
                workflow,
                job,
            } => serde_json::to_value(
                self.queue
                    .admit_job(&worker, id, &generation, &workflow, job)
                    .await?,
            )?,
            Operation::FinishJob { id, job } => {
                serde_json::to_value(self.queue.finish_job(&worker, id, &job).await?)?
            }
            Operation::Heartbeat { id } => {
                serde_json::to_value(self.queue.heartbeat(&worker, id).await?)?
            }
            Operation::Logs {
                id,
                sequence,
                lines,
            } => serde_json::to_value(self.queue.logs(&worker, id, sequence, &lines).await?)?,
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
    /// node must be an active member with its own key, and the caller an active runner.
    /// A check that cannot read the database refuses too.
    pub async fn refusal(&self, peer: iroh::PublicKey) -> Option<&'static str> {
        let checked = async {
            let pool = self.context.database()?;
            let serving = codeyard_db_cluster::cluster::get_node_by_id(pool, self.context.node_id)
                .await?
                .is_some_and(|node| {
                    node.status == "active" && node.public_key == self.context.key.public_hex()
                });
            let worker =
                codeyard_db_cluster::cluster::get_node_by_public_key(pool, &peer.to_string())
                    .await?
                    .is_some_and(|node| {
                        node.status == "active" && node.roles.iter().any(|role| role == "runner")
                    });
            Ok::<_, anyhow::Error>((serving, worker))
        };
        match checked.await {
            Ok((true, true)) => None,
            Ok((true, false)) => Some("not an active runner"),
            Ok((false, _)) => Some(CORE_UNAVAILABLE),
            Err(error) => {
                tracing::warn!(%error, "CI queue membership check failed");
                Some(CORE_UNAVAILABLE)
            }
        }
    }

    /// The reply to the request on `link`. An error means the frame is broken, and it ends
    /// the connection instead.
    pub async fn answer(
        &self,
        link: &mut Link,
        slot: Arc<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<Result<serde_json::Value, String>> {
        let peer = link.peer;
        if let Some(refusal) = self.refusal(peer).await {
            return Ok(Err(refusal.to_owned()));
        }
        tracing::trace!(name: "ci_request_authorized", %peer, "CI queue request authorized; waiting for frame");
        let header = codeyard_repos_core::wire::read_frame(
            &mut link.reader,
            codeyard_db_core::limits::MAX_JSON + 64 * 1024,
        )
        .await?;
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
            if let Err(error) =
                codeyard_db_cluster::cluster::touch_node(pool, &peer.to_string()).await
            {
                tracing::debug!(%error, "could not record a worker's last request");
            }
        }
        let handler = self.clone();
        let result = self
            .sessions
            .spawn_admitted(async move {
                let _slot = slot;
                handler.execute(peer, request).await
            })
            .await?;
        Ok(match result {
            Ok(value) => Ok(value),
            Err(error) => {
                // Never return database errors or decrypted secret contents to the peer.
                tracing::warn!(%error, "CI queue request failed");
                Err(
                    if error.to_string() == CORE_UNAVAILABLE || transient_database_error(&error) {
                        CORE_UNAVAILABLE.to_owned()
                    } else {
                        REFUSED.to_owned()
                    },
                )
            }
        })
    }

    /// Answers the one request on `link`.
    pub(crate) async fn serve_link(&self, link: Link) -> Result<()> {
        let Some(request) = self.sessions.admit(self.answer_link(link)) else {
            return Ok(());
        };
        request.await
    }

    async fn answer_link(&self, mut link: Link) -> Result<()> {
        let _header = codeyard_cluster_core::transport::peer_header_slot(&self.headers, link.peer)?;
        tokio::time::timeout(std::time::Duration::from_secs(90), async {
            // A busy core refuses at once, as pairing does, so the client asks another
            // core instead of waiting in a queue. Hold admission through the reply write, and
            // in the request task, which keeps running if this reply is cancelled.
            let slot = self.slots.clone().try_acquire_owned().map(Arc::new);
            let result = match &slot {
                Ok(slot) => self.answer(&mut link, slot.clone()).await?,
                Err(_) => Err(CORE_UNAVAILABLE.to_owned()),
            };
            let header = serde_json::to_vec(&Reply { result })?;
            codeyard_repos_core::wire::write_frame(
                &mut link.writer,
                &header,
                codeyard_db_core::limits::MAX_JSON,
            )
            .await?;
            link.finish().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }
}

impl ProtocolHandler for CiHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let Ok(_connection) = codeyard_cluster_core::transport::connection_slot(&connection) else {
            return Ok(());
        };
        while let Some(link) = codeyard_cluster_core::transport::accept_stream(&connection).await {
            if let Err(error) = self.serve_link(link).await {
                tracing::warn!(%error, "CI queue RPC failed");
                connection.close(500u32.into(), b"CI request failed");
                break;
            }
        }
        Ok(())
    }
}
