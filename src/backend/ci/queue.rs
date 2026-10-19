//! One CI queue, reached through the database or a paired core node.
use super::{CiConfig, CiExecutionMode};
use crate::backend::{
    cluster::{self, rpc, NodeContext},
    crypto::SecretEncryptor,
    db::{self, models::CiRun, DbPool},
    notifications::NotificationBroadcaster,
    repositories::store::{repo_store, RepoStore},
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

/// The pull request a `pull_request` run belongs to. It becomes `github.event`.
#[derive(Clone, Serialize, Deserialize)]
pub struct PullRequestEvent {
    pub number: i32,
    pub title: String,
    pub head_ref: String,
    pub base_ref: String,
    pub base_sha: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ClaimedRun {
    pub run: CiRun,
    pub storage_node_id: Option<i64>,
    pub repository: Uuid,
    pub repository_name: String,
    pub owner: String,
    /// Login of the user who started the run; empty when no account did.
    pub actor: String,
    pub secrets: Vec<(String, String)>,
    #[serde(default)]
    pub pull_request: Option<PullRequestEvent>,
    pub storage_peer: Option<cluster::CorePeer>,
    // Used only by a database worker; never sent to a remote worker.
    #[serde(skip)]
    pub(crate) store: Option<RepoStore>,
}

#[derive(Clone)]
pub enum RunQueue {
    Database {
        pool: DbPool,
        config: Arc<CiConfig>,
        secrets: Option<Arc<SecretEncryptor>>,
        context: Option<NodeContext>,
        notifications: Arc<NotificationBroadcaster>,
    },
    Remote {
        context: NodeContext,
        peers: Vec<cluster::CorePeer>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub stage: String,
    pub line: String,
    pub level: String,
}

/// Replaces each control byte except tab, CR and LF with `?`. PostgreSQL cannot store NUL.
/// A replacement keeps the byte length, so the batch limits and the worker's byte count
/// match what is stored.
fn printable_log_text(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' | '\r' | '\n' => c,
            c if c.is_ascii_control() => '?',
            c => c,
        })
        .collect()
}

pub(crate) fn prepare_logs(lines: &[LogLine]) -> Result<Vec<LogLine>> {
    ensure!(
        !lines.is_empty() && lines.len() <= 64,
        "CI log batch exceeds ingestion limits"
    );
    // PostgreSQL text cannot store NUL. Count the printable bytes on both engines.
    let lines: Vec<_> = lines
        .iter()
        .map(|line| LogLine {
            stage: printable_log_text(&line.stage),
            line: printable_log_text(&line.line),
            level: line.level.clone(),
        })
        .collect();
    ensure!(
        lines.iter().all(|line| line.stage.len() <= 255
            && line.line.len() <= 16 * 1024
            && matches!(line.level.as_str(), "info" | "warn" | "error" | "debug")),
        "CI log input exceeds ingestion limits"
    );
    ensure!(
        lines
            .iter()
            .map(|line| line.line.len() + line.stage.len())
            .sum::<usize>()
            <= 256 * 1024,
        "CI log batch exceeds ingestion limits"
    );
    Ok(lines)
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Operation {
    Claim {
        mode: CiExecutionMode,
    },
    Heartbeat {
        id: i64,
    },
    Logs {
        id: i64,
        lines: Vec<LogLine>,
    },
    Finish {
        id: i64,
        status: String,
        message: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Request {
    pub worker: String,
    pub operation: Operation,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Reply {
    pub result: std::result::Result<serde_json::Value, String>,
}

/// The reply of a core node that cannot serve CI now: it is not an active member with its
/// own key, or it is busy. The caller then asks the next core.
pub(crate) const CORE_UNAVAILABLE: &str = "core node unavailable";

impl RunQueue {
    pub fn store(&self, claim: &ClaimedRun) -> Result<RepoStore> {
        match self {
            Self::Database { .. } => claim
                .store
                .clone()
                .context("the claim has no repository store"),
            Self::Remote { context, .. } => {
                let peer = claim
                    .storage_peer
                    .as_ref()
                    .context("storage node unavailable")?;
                let id = claim.storage_node_id.context("storage node unavailable")?;
                let worker = claim
                    .run
                    .worker
                    .as_deref()
                    .context("the CI run has no worker")?;
                context.remember_peer(id, peer.clone());
                Ok(RepoStore::remote(claim.repository, id, context)
                    .for_ci_run(claim.run.id, worker))
            }
        }
    }

    pub async fn claim(&self, worker: &str, mode: CiExecutionMode) -> Result<Option<ClaimedRun>> {
        self.claim_with_stop(worker, mode, None).await
    }

    /// `claim` for a worker that may be shutting down. A remote claim that is still dialing when
    /// `stop` is set gives up without sending its request. Once the request is sent the claim is
    /// awaited: the core may already have committed a run for this worker, and only its answer
    /// says so.
    pub async fn claim_with_stop(
        &self,
        worker: &str,
        mode: CiExecutionMode,
        stop: Option<&mut watch::Receiver<bool>>,
    ) -> Result<Option<ClaimedRun>> {
        match self {
            Self::Remote { .. } => self.call(worker, Operation::Claim { mode }, stop).await,
            Self::Database {
                pool,
                config,
                secrets,
                context,
                ..
            } => {
                let trusted: Vec<_> = config.trusted_actor_ids.iter().copied().collect();
                let claimed = match context {
                    // Cluster worker ids start with the node's public key, then `/`.
                    Some(_) => {
                        let node_key = worker.split_once('/').map_or(worker, |(key, _)| key);
                        db::ci::claim_ci_run_on_node(
                            pool,
                            node_key,
                            worker,
                            mode.as_str(),
                            &trusted,
                        )
                        .await?
                    }
                    // A database without cluster membership has no node rows to check.
                    None => db::ci::claim_ci_run(pool, worker, mode.as_str(), &trusted).await?,
                };
                let Some(run) = claimed else {
                    return Ok(None);
                };
                let loaded = async {
                    let repository =
                        db::repositories::records::get_repository_by_id(pool, run.repo_id)
                            .await?
                            .context("the repository no longer exists")?;
                    let uuid = rpc::repository_uuid(&repository.disk_path)?;
                    let owner = db::ci::ci_repository_owner(pool, run.repo_id)
                        .await?
                        .unwrap_or_default();
                    let actor = match run.triggered_by_id {
                        Some(id) => db::users::accounts::get_user_by_id(pool, id)
                            .await?
                            .map(|user| user.username)
                            .unwrap_or_default(),
                        None => String::new(),
                    };
                    let pull_request = run
                        .pull_request_event
                        .clone()
                        .map(serde_json::from_value)
                        .transpose()?;
                    let secrets = match secrets {
                        Some(encryptor) if !run.is_fork => {
                            db::secrets::decrypt_repository_secrets(pool, encryptor, run.repo_id)
                                .await?
                        }
                        None if !run.is_fork => {
                            ensure!(
                                !db::secrets::repository_has_secrets(pool, run.repo_id).await?,
                                "repository secrets cannot be decrypted without SECRETS_MASTER_KEY"
                            );
                            Vec::new()
                        }
                        _ => Vec::new(),
                    };
                    // The whole set goes out in one RPC reply and into one secret file, so it
                    // is bounded before the run is handed out.
                    db::secrets::validate_repository_secrets(&secrets)?;
                    let storage_id = repository
                        .storage_node_id
                        .or_else(|| context.as_ref().map(|c| c.node_id));
                    let storage_peer = match storage_id {
                        Some(id) => {
                            let node = db::cluster::get_node_by_id(pool, id)
                                .await?
                                .filter(|node| {
                                    node.status == "active"
                                        && node.roles.iter().any(|r| r == "storage")
                                })
                                .context("storage node unavailable")?;
                            Some(cluster::CorePeer {
                                public_key: node.public_key,
                                address: serde_json::from_str(
                                    node.addr_hint
                                        .as_deref()
                                        .context("storage node unavailable")?,
                                )?,
                            })
                        }
                        None => None,
                    };
                    let worker = run.worker.as_deref().context("the CI run has no worker")?;
                    let store =
                        Some(repo_store(&repository, context.as_ref()).for_ci_run(run.id, worker));
                    Ok::<_, anyhow::Error>(ClaimedRun {
                        run: run.clone(),
                        storage_node_id: repository.storage_node_id,
                        repository: uuid,
                        repository_name: repository.name,
                        owner,
                        actor,
                        secrets,
                        pull_request,
                        storage_peer,
                        store,
                    })
                }
                .await;
                if loaded.is_err() {
                    self.finish(
                        worker,
                        run.id,
                        "failed",
                        Some("CI could not load the repository or secrets"),
                    )
                    .await?;
                }
                loaded.map(Some)
            }
        }
    }

    pub async fn heartbeat(&self, worker: &str, id: i64) -> Result<Option<bool>> {
        match self {
            Self::Database { pool, .. } => db::ci::heartbeat_ci_run(pool, id, worker).await,
            Self::Remote { .. } => self.call(worker, Operation::Heartbeat { id }, None).await,
        }
    }

    pub async fn logs(&self, worker: &str, id: i64, lines: &[LogLine]) -> Result<()> {
        let lines = prepare_logs(lines)?;
        match self {
            Self::Database { pool, .. } => db::ci::append_ci_logs(pool, id, worker, &lines).await,
            Self::Remote { .. } => self.call(worker, Operation::Logs { id, lines }, None).await,
        }
    }

    pub async fn finish(
        &self,
        worker: &str,
        id: i64,
        status: &str,
        message: Option<&str>,
    ) -> Result<bool> {
        ensure!(
            matches!(status, "success" | "failed" | "cancelled"),
            "invalid CI terminal status"
        );
        ensure!(
            message.is_none_or(|m| m.len() <= 16 * 1024),
            "CI result exceeds ingestion limits"
        );
        match self {
            Self::Database {
                pool,
                notifications,
                ..
            } => {
                let finished = db::ci::finish_ci_run(pool, id, worker, status, message).await?;
                if let Some((_, created)) = &finished {
                    notifications.send_created(created).await;
                }
                Ok(finished.is_some())
            }
            Self::Remote { .. } => {
                self.call(
                    worker,
                    Operation::Finish {
                        id,
                        status: status.into(),
                        message: message.map(str::to_owned),
                    },
                    None,
                )
                .await
            }
        }
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        worker: &str,
        operation: Operation,
        mut stop: Option<&mut watch::Receiver<bool>>,
    ) -> Result<T> {
        let Self::Remote { context, peers } = self else {
            unreachable!()
        };
        // Log appends are not idempotent. Once sent, a lost reply must not replay the batch.
        let replay_safe = !matches!(operation, Operation::Logs { .. });
        let header = serde_json::to_vec(&Request {
            worker: worker.into(),
            operation,
        })?;
        let mut last = anyhow::anyhow!("core node unavailable");
        for peer in peers {
            let node = peer.node()?;
            // dial_node bounds each attempt itself. An outer timeout here would cut the
            // fallback short, so the other addresses and iroh would never be tried. A stop
            // ends only the dial: once the request is written, its reply is awaited below.
            let dial = cluster::dial_node(&context.iroh, node, &peer.address, cluster::ALPN_EVENTS);
            let dialed = match stop.as_deref_mut() {
                Some(stop) => {
                    // The stop wins a tie, and is read again once the dial is ready, so a
                    // stopping worker never writes a request that could claim a run.
                    let dialed = tokio::select! {
                        biased;
                        _ = stop.wait_for(|stopped| *stopped) => None,
                        dialed = dial => Some(dialed),
                    };
                    match dialed {
                        Some(dialed) if !*stop.borrow() => dialed,
                        _ => {
                            return Err(anyhow::anyhow!(
                                "CI worker stopped before reaching the core"
                            ));
                        }
                    }
                }
                None => dial.await,
            };
            let Ok(mut link) = dialed else {
                continue;
            };
            let result = tokio::time::timeout(std::time::Duration::from_secs(20), async {
                rpc::write_frame(&mut link.writer, &header, rpc::MAX_JSON).await?;
                link.finish().await?;
                let reply: Reply = serde_json::from_slice(
                    &rpc::read_frame(&mut link.reader, rpc::MAX_JSON).await?,
                )?;
                Ok::<_, anyhow::Error>(reply)
            })
            .await;
            link.close(0, b"CI request complete");
            match result {
                Ok(Ok(reply)) => match reply.result {
                    Ok(value) => return serde_json::from_value(value).map_err(Into::into),
                    Err(error) if error == CORE_UNAVAILABLE => {
                        last = anyhow::anyhow!(CORE_UNAVAILABLE)
                    }
                    Err(error) => return Err(anyhow::Error::msg(error)),
                },
                Ok(Err(error)) => {
                    if !replay_safe {
                        return Err(error);
                    }
                    last = error;
                }
                Err(_) => {
                    last = anyhow::anyhow!("core node request timed out");
                    if !replay_safe {
                        return Err(last);
                    }
                }
            }
        }
        Err(last)
    }
}
